//! Real p2p transport via a **p2panda-net node** (p2p P3, `ring-p2p` feature).
//!
//! This is the non-loopback backing for the [`P2p`](noeta_stdlib::P2p) host capability: where the
//! sandbox (and the default `RealHost`) use a deterministic in-process broker, a build with the
//! `ring-p2p` ring gives `RealHost` a genuine p2panda-net node — gossip pub/sub over iroh/QUIC with
//! mDNS discovery. Non-deterministic and CLI-only, exactly like `reqwest` for `Network`; never
//! oracle-tested.
//!
//! # The async bridge (the one genuinely new piece)
//!
//! A p2panda node is **long-lived** — it runs discovery/gossip background tasks continuously — while
//! the `P2p` trait surface is **synchronous** (`p2p_publish`, `p2p_poll_sub`, …). The bridge, modelled
//! on how `RealHost` already holds the HTTP server's long-lived listener and on `noeta-reactive`'s
//! "one long-lived thing owned by the scope, lazily started, released at teardown":
//!
//! - The node owns a **dedicated multi-thread tokio runtime**. Its worker threads keep the node's
//!   spawned tasks running between our synchronous calls (a `current_thread` `block_on`-per-call
//!   runtime could not — that is why `RealHost`'s own runtime is not reused).
//! - Each **subscription** spawns a drain task on that runtime forwarding the gossip stream into an
//!   unbounded channel; `p2p_poll_sub` is a non-blocking `try_recv` off that channel (no runtime
//!   needed), so the sync trait method reads real network data with no `.await`.
//! - The node is started **lazily** on first p2p use and lives until the isolate's `RealHost` drops,
//!   which drops the runtime and severs the tasks (residency returns to zero — the reactive lesson).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use futures_util::StreamExt;
use p2panda_core::Hash;
use p2panda_net::gossip::GossipHandle;
use p2panda_net::iroh_mdns::MdnsDiscoveryMode;
use p2panda_net::{AddressBook, Discovery, Endpoint, Gossip, MdnsDiscovery};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::io_error;
use noeta_stdlib::StdError;

/// A long-lived p2panda-net node bridging the async gossip overlay to the synchronous [`P2p`]
/// capability. One per `RealHost` (per isolate), started lazily.
pub struct P2pNode {
    /// The node's own multi-thread runtime; keeps its background tasks alive between our sync calls.
    /// Dropped last (its `Drop` shuts the tasks down) — declared last so field-drop order agrees.
    runtime: Runtime,
    /// The gossip overlay handle — `stream(topic)` joins a topic.
    gossip: Gossip,
    /// One joined-topic handle per topic name, so repeat publishes/subscribes reuse the membership.
    handles: Mutex<HashMap<String, GossipHandle>>,
    /// subscription id → the channel a drain task feeds from that subscription's gossip stream.
    subs: Mutex<HashMap<u64, UnboundedReceiver<Vec<u8>>>>,
    next_sub: AtomicU64,
    /// topic → the single default subscription backing the topic-level `p2p.receive` (P1's default
    /// reader), created lazily so `poll_default` mirrors the broker's one-implicit-reader semantics.
    default_subs: Mutex<HashMap<String, u64>>,
    // Discovery/endpoint components: kept alive for the node's lifetime (their background tasks run
    // on `runtime`), otherwise unused directly.
    _endpoint: Endpoint,
    _address_book: AddressBook,
    _discovery: Discovery,
    /// mDNS is best-effort — `None` when the environment has no usable multicast (a sandbox/CI),
    /// in which case the node still works over manually-wired or relay-discovered peers.
    _mdns: Option<MdnsDiscovery>,
}

impl std::fmt::Debug for P2pNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pNode")
            .field("topics", &self.handles.lock().map(|h| h.len()).unwrap_or(0))
            .field("subscriptions", &self.subs.lock().map(|s| s.len()).unwrap_or(0))
            .finish_non_exhaustive()
    }
}

impl P2pNode {
    /// Build and start the node (blocking until its components are up). Fails only if the runtime or
    /// the core networking components (endpoint, gossip) cannot start; mDNS failure is tolerated.
    pub fn start() -> Result<P2pNode, StdError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| io_error(format!("cannot start the p2p node runtime: {e}")))?;

        let node = runtime.block_on(async {
            let address_book = AddressBook::builder()
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p address book: {e}")))?;
            let endpoint = Endpoint::builder(address_book.clone())
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p endpoint: {e}")))?;
            // Discovery of peers interested in the same topic (confidential PSI over the endpoint).
            let discovery = Discovery::builder(address_book.clone(), endpoint.clone())
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p discovery: {e}")))?;
            // mDNS: LAN discovery. Best-effort — a container without multicast simply gets no mDNS.
            let mdns = match MdnsDiscovery::builder(address_book.clone(), endpoint.clone())
                .mode(MdnsDiscoveryMode::Active)
                .spawn()
                .await
            {
                Ok(mdns) => Some(mdns),
                Err(e) => {
                    eprintln!("noeta p2p: mDNS discovery unavailable ({e}); continuing without it");
                    None
                }
            };
            let gossip = Gossip::builder(address_book.clone(), endpoint.clone())
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p gossip: {e}")))?;
            Ok::<_, StdError>((address_book, endpoint, discovery, mdns, gossip))
        })?;
        let (address_book, endpoint, discovery, mdns, gossip) = node;

        Ok(P2pNode {
            runtime,
            gossip,
            handles: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            next_sub: AtomicU64::new(0),
            default_subs: Mutex::new(HashMap::new()),
            _endpoint: endpoint,
            _address_book: address_book,
            _discovery: discovery,
            _mdns: mdns,
        })
    }

    /// A topic name → p2panda [`Topic`](p2panda_core::Topic): the 32-byte hash of the name, so any
    /// string is a valid topic and two nodes naming the same string join the same overlay.
    fn topic_of(name: &str) -> p2panda_core::Topic {
        Hash::digest(name.as_bytes()).into()
    }

    /// The joined-topic handle for `topic`, joining (async) on first use and caching it so the
    /// overlay membership persists for the node's lifetime.
    fn handle_for(&self, topic: &str) -> Result<GossipHandle, StdError> {
        if let Some(handle) = self.handles.lock().unwrap().get(topic) {
            return Ok(handle.clone());
        }
        let handle = self
            .runtime
            .block_on(self.gossip.stream(Self::topic_of(topic)))
            .map_err(|e| io_error(format!("cannot join p2p topic `{topic}`: {e}")))?;
        self.handles
            .lock()
            .unwrap()
            .insert(topic.to_string(), handle.clone());
        Ok(handle)
    }

    /// Broadcast `message` to everyone in `topic`'s gossip overlay (ephemeral — a peer that is
    /// offline or subscribes later will not see it; that is what the sync/log layer is for, P3.2).
    pub fn publish(&self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        let handle = self.handle_for(topic)?;
        self.runtime
            .block_on(handle.publish(message))
            .map_err(|e| io_error(format!("cannot publish to p2p topic `{topic}`: {e}")))
    }

    /// Subscribe to `topic`; a drain task forwards its gossip stream into a channel, and the id
    /// returned is polled via [`Self::poll_sub`]. Ephemeral: only messages published *after* this
    /// call arrive (a gossip `subscribe` starts from now).
    pub fn subscribe(&self, topic: &str) -> Result<u64, StdError> {
        let handle = self.handle_for(topic)?;
        let mut stream = handle.subscribe();
        let (tx, rx) = mpsc::unbounded_channel();
        // Runs on the node's runtime for the node's lifetime; ends when the receiver is dropped.
        self.runtime.spawn(async move {
            while let Some(item) = stream.next().await {
                // A stream error (a lagged broadcast receiver) is skipped, not fatal.
                if let Ok(bytes) = item
                    && tx.send(bytes).is_err()
                {
                    break; // receiver gone — nothing more to deliver
                }
            }
        });
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        self.subs.lock().unwrap().insert(id, rx);
        Ok(id)
    }

    /// The next message pending on subscription `sub` (non-blocking), or `None` if none has arrived
    /// or the id is unknown.
    pub fn poll_sub(&self, sub: u64) -> Option<Vec<u8>> {
        let mut subs = self.subs.lock().unwrap();
        subs.get_mut(&sub).and_then(|rx| rx.try_recv().ok())
    }

    /// The next message on `topic`'s **default** reader (backing the ephemeral `p2p.receive`), the
    /// node analogue of the broker's single per-topic cursor: one subscription per topic, created on
    /// first poll.
    pub fn poll_default(&self, topic: &str) -> Result<Option<Vec<u8>>, StdError> {
        // Read the existing id and release the lock *before* the match — `subscribe` (in the miss
        // arm) re-locks `default_subs`, and holding the guard across it would self-deadlock (the
        // `std::sync::Mutex` is non-reentrant).
        let existing = self.default_subs.lock().unwrap().get(topic).copied();
        let sub = match existing {
            Some(id) => id,
            None => {
                let id = self.subscribe(topic)?;
                self.default_subs
                    .lock()
                    .unwrap()
                    .insert(topic.to_string(), id);
                id
            }
        };
        Ok(self.poll_sub(sub))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node boots and its gossip pipeline runs: join a topic, publish (to no one), subscribe,
    /// and confirm a non-blocking poll is empty. Real cross-node delivery is exercised by the
    /// two-node integration test (P3.1); this pins that the async bridge itself works.
    #[test]
    fn node_starts_and_the_gossip_pipeline_runs() {
        let node = P2pNode::start().expect("node starts");
        node.publish("room", b"hello".to_vec())
            .expect("publish to an empty overlay succeeds");
        let sub = node.subscribe("room").expect("subscribe succeeds");
        // No peers, so nothing is delivered — but the poll path must work and be empty.
        assert_eq!(node.poll_sub(sub), None);
        assert_eq!(node.poll_sub(999), None); // unknown subscription id
    }
}
