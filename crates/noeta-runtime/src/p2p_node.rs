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
use p2panda_core::{Body, Hash, Header, Operation, SeqNum, SigningKey};
use p2panda_net::gossip::GossipHandle;
use p2panda_net::iroh_mdns::MdnsDiscoveryMode;
use p2panda_net::sync::SyncHandle;
use p2panda_net::{AddressBook, Discovery, Endpoint, Gossip, LogSync, MdnsDiscovery};
use p2panda_store::operations::OperationStore;
use p2panda_store::topics::TopicStore;
use p2panda_store::{SqliteStore, SqliteStoreBuilder, Transaction};
use p2panda_sync::protocols::TopicLogSyncEvent;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::io_error;
use noeta_stdlib::StdError;

/// Every node keeps a single append-only log for its own operations; the durable (log-sync)
/// transport hard-codes its id, matching p2panda's `chat` example.
type LogId = u64;
const LOG_ID: LogId = 1;

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

    // --- Durable (log-sync) transport (p2p P3.2), backing synced_signal ---
    /// This node's Ed25519 identity — signs every operation it appends to its log, and is the
    /// endpoint's key, so a peer attributes received operations to this author.
    signing_key: SigningKey,
    /// The append-only operation log store (in-memory SQLite). Holds this node's log and peers'
    /// synced logs, giving a late-joining replica the full history to converge from.
    store: SqliteStore,
    /// The log-sync engine (over `store` + the endpoint + gossip). Joins a topic via `stream`.
    log_sync: LogSync<SqliteStore, LogId, ()>,
    /// topic → its durable state: the sync handle (join), plus this author's log tip (seq + backlink)
    /// so appends stay a correct append-only chain.
    durable: Mutex<HashMap<String, DurableTopic>>,
    // Discovery/endpoint components: kept alive for the node's lifetime (their background tasks run
    // on `runtime`), otherwise unused directly.
    _endpoint: Endpoint,
    _address_book: AddressBook,
    _discovery: Discovery,
    /// mDNS is best-effort — `None` when the environment has no usable multicast (a sandbox/CI),
    /// in which case the node still works over manually-wired or relay-discovered peers.
    _mdns: Option<MdnsDiscovery>,
}

/// One topic's durable (log-sync) state: the sync handle it was joined with, plus this author's
/// log tip so the next append links correctly.
struct DurableTopic {
    handle: SyncHandle<Operation, TopicLogSyncEvent>,
    seq_num: SeqNum,
    backlink: Option<Hash>,
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

        // This node's persistent-for-the-session identity (ephemeral in P3.2 — persisting it to
        // disk is P3.3). It signs the endpoint AND every log operation, so peers attribute synced
        // operations to this author.
        let signing_key = SigningKey::generate();

        let node = runtime.block_on(async {
            let address_book = AddressBook::builder()
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p address book: {e}")))?;
            let endpoint = Endpoint::builder(address_book.clone())
                .signing_key(signing_key.clone())
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
            // The durable transport: an in-memory append-log store + the log-sync engine over the
            // same endpoint/gossip. `synced_signal` publishes/subscribes through this. (In-memory
            // for P3.2 — a persistent on-disk store for true offline-restart is a P3.3 config.)
            let store = SqliteStoreBuilder::memory()
                .build()
                .await
                .map_err(|e| io_error(format!("p2p store: {e}")))?;
            let log_sync = LogSync::<_, LogId, _>::builder(store.clone(), endpoint.clone(), gossip.clone())
                .spawn()
                .await
                .map_err(|e| io_error(format!("p2p log-sync: {e}")))?;
            Ok::<_, StdError>((address_book, endpoint, discovery, mdns, gossip, store, log_sync))
        })?;
        let (address_book, endpoint, discovery, mdns, gossip, store, log_sync) = node;

        Ok(P2pNode {
            runtime,
            gossip,
            handles: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            next_sub: AtomicU64::new(0),
            default_subs: Mutex::new(HashMap::new()),
            signing_key,
            store,
            log_sync,
            durable: Mutex::new(HashMap::new()),
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

    // --- Durable transport (p2p P3.2): log-sync, backing synced_signal --------------------------

    /// Join the topic's log-sync stream (idempotent): associate this author's log with the topic in
    /// the store, then open the sync stream in live mode. Cached in `durable`.
    fn ensure_durable(&self, topic: &str) -> Result<(), StdError> {
        if self.durable.lock().unwrap().contains_key(topic) {
            return Ok(());
        }
        let handle = self.runtime.block_on(async {
            let permit = self
                .store
                .begin()
                .await
                .map_err(|e| io_error(format!("p2p store: {e}")))?;
            self.store
                .associate(&Self::topic_of(topic), &self.signing_key.verifying_key(), &LOG_ID)
                .await
                .map_err(|e| io_error(format!("p2p store associate: {e}")))?;
            self.store
                .commit(permit)
                .await
                .map_err(|e| io_error(format!("p2p store commit: {e}")))?;
            self.log_sync
                .stream(Self::topic_of(topic), true)
                .await
                .map_err(|e| io_error(format!("p2p log-sync join `{topic}`: {e}")))
        })?;
        self.durable
            .lock()
            .unwrap()
            .entry(topic.to_string())
            .or_insert(DurableTopic {
                handle,
                seq_num: 0,
                backlink: None,
            });
        Ok(())
    }

    /// Durable publish: append `message` as a signed operation to this author's log (persisted in
    /// the store), then hand it to log-sync — delivered to current peers *and* to any peer that
    /// syncs later. This is the eventual-consistency guarantee `synced_signal` relies on.
    pub fn publish_durable(&self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.ensure_durable(topic)?;
        let mut durable = self.durable.lock().unwrap();
        let entry = durable.get_mut(topic).expect("ensured above");
        let body = Body::new(&message);
        let (hash, operation) =
            create_operation(&self.signing_key, &body, entry.seq_num, entry.backlink);
        self.runtime.block_on(async {
            let permit = self
                .store
                .begin()
                .await
                .map_err(|e| io_error(format!("p2p store: {e}")))?;
            self.store
                .insert_operation(&hash, &operation, &LOG_ID)
                .await
                .map_err(|e| io_error(format!("p2p store insert: {e}")))?;
            self.store
                .commit(permit)
                .await
                .map_err(|e| io_error(format!("p2p store commit: {e}")))
        })?;
        entry
            .handle
            .publish(operation)
            .map_err(|e| io_error(format!("p2p log-sync publish: {e}")))?;
        entry.seq_num += 1;
        entry.backlink = Some(hash);
        Ok(())
    }

    /// Durable subscribe: drain the topic's log-sync stream, forwarding each received operation's
    /// payload into a channel `poll_sub` reads (same id space as gossip subscriptions).
    pub fn subscribe_durable(&self, topic: &str) -> Result<u64, StdError> {
        self.ensure_durable(topic)?;
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let durable = self.durable.lock().unwrap();
            let entry = durable.get(topic).expect("ensured above");
            let mut stream = self
                .runtime
                .block_on(entry.handle.subscribe())
                .map_err(|e| io_error(format!("p2p log-sync subscribe: {e}")))?;
            self.runtime.spawn(async move {
                while let Some(Ok(from_sync)) = stream.next().await {
                    // Only new operations carry a payload to merge; session-lifecycle events are
                    // ignored here (a `SyncStatus` surface consumes them in a later slice).
                    if let TopicLogSyncEvent::OperationReceived { operation, .. } = from_sync.event
                        && let Some(body) = operation.body
                        && tx.send(body.to_bytes()).is_err()
                    {
                        break; // receiver gone
                    }
                }
            });
        }
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        self.subs.lock().unwrap().insert(id, rx);
        Ok(id)
    }
}

/// Build a signed, sequence-numbered, back-linked operation for this author's log (verbatim from
/// p2panda's `chat` example): the append-only-log entry that log-sync distributes.
fn create_operation(
    signing_key: &SigningKey,
    body: &Body,
    seq_num: SeqNum,
    backlink: Option<Hash>,
) -> (Hash, Operation) {
    let mut header = Header {
        version: 1,
        verifying_key: signing_key.verifying_key(),
        signature: None,
        payload_size: body.size(),
        payload_hash: Some(body.hash()),
        seq_num,
        backlink,
        extensions: (),
    };
    header.sign(signing_key);
    let hash = header.hash();
    let operation = Operation {
        hash,
        header,
        body: Some(body.to_owned()),
    };
    (hash, operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node boots and its gossip pipeline runs: join a topic, publish (to no one), subscribe,
    /// and confirm a non-blocking poll is empty. Real cross-node delivery is exercised by the
    /// two-node integration test (P3.1); this pins that the async bridge itself works.
    /// Two real nodes on the same topic (discovered over mDNS) exchange a gossip message. Not
    /// hermetic — needs real multicast/networking — so `#[ignore]`, run explicitly:
    /// `cargo test -p noeta-runtime --features ring-p2p -- --ignored two_nodes`.
    /// The durable (log-sync) catch-up guarantee that gossip lacks: node A publishes **before**
    /// node B exists, and B still receives it once it joins and syncs A's log. Not hermetic (real
    /// networking) — run explicitly:
    /// `cargo test -p noeta-runtime --features ring-p2p -- --ignored durable_catch_up`.
    #[test]
    #[ignore = "needs real networking (mDNS multicast); run explicitly"]
    fn durable_catch_up_reaches_a_late_joiner() {
        let a = P2pNode::start().expect("node a");
        // A subscribes (joins the overlay) and publishes durably — this goes into A's log.
        let _sub_a = a.subscribe_durable("room").expect("a subscribes");
        a.publish_durable("room", b"durable state".to_vec())
            .expect("a publishes durably");
        std::thread::sleep(std::time::Duration::from_secs(2));

        // B starts LATE — after A already published — and subscribes. Over gossip it would miss
        // the message; over log-sync it syncs A's log and catches up.
        let b = P2pNode::start().expect("node b");
        let sub_b = b.subscribe_durable("room").expect("b subscribes");

        let mut received = None;
        for _ in 0..200 {
            if let Some(bytes) = b.poll_sub(sub_b) {
                received = Some(bytes);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(received.as_deref(), Some(&b"durable state"[..]));
    }

    #[test]
    #[ignore = "needs real networking (mDNS multicast); run explicitly"]
    fn two_nodes_exchange_a_gossip_message() {
        let a = P2pNode::start().expect("node a");
        let b = P2pNode::start().expect("node b");
        // Both subscribe first (gossip is ephemeral — only messages published after subscribing
        // arrive), then give discovery a moment to connect the overlay.
        let sub_b = b.subscribe("room").expect("b subscribes");
        let _sub_a = a.subscribe("room").expect("a subscribes");
        std::thread::sleep(std::time::Duration::from_secs(3));

        a.publish("room", b"hi from a".to_vec()).expect("a publishes");

        // Poll b for up to ~15s (discovery + delivery are not instant).
        let mut received = None;
        for _ in 0..150 {
            if let Some(bytes) = b.poll_sub(sub_b) {
                received = Some(bytes);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(received.as_deref(), Some(&b"hi from a"[..]));
    }

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
