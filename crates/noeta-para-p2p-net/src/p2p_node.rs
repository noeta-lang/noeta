//! Real p2p transport via a **p2panda-net node** (p2p P3, `ring-p2p` feature).
//!
//! This is the non-loopback backing for the [`P2p`](noeta_ext_abi::host::P2p) host capability: where the
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

use std::collections::VecDeque;
use std::str::FromStr;

use p2panda_core::VerifyingKey;
use p2panda_encryption::Rng;
use p2panda_encryption::crypto::x25519::SecretKey;
use p2panda_spaces::{ActorId, Credentials};

use crate::io_error;
use crate::p2p_crypto::{CryptoGroups, EncryptedGroup};
use noeta_ext_abi::{StdError, SyncStatus};

/// Every node keeps a single append-only log for its own operations; the durable (log-sync)
/// transport hard-codes its id, matching p2panda's `chat` example.
type LogId = u64;
const LOG_ID: LogId = 1;

/// Node configuration (p2p P3.3/P3.4). Where the node's persistent state lives, and (P3.4) how it
/// discovers peers. `Default` resolves the per-user XDG data dir, so `noeta run` of a p2p program
/// keeps its identity and synced logs across restarts with no configuration.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    /// An explicit directory holding this node's identity (`identity.key`) and durable store
    /// (`store.db`). `None` ⇒ the per-user default *for this app* (see [`Self::app_id`]). Two
    /// distinct replicas on one machine (e.g. an integration test) point at two dirs; the *same*
    /// replica reuses one dir to restart with its prior identity + state.
    pub data_dir: Option<PathBuf>,
    /// The **application namespace** the default data dir is keyed on, so two different Noeta apps
    /// never share one p2p identity/store and clobber each other. Resolved (when `data_dir` is
    /// `None`) as `$XDG_DATA_HOME/<app>/p2p`, where `<app>` is `$NOETA_P2P_APP` if set, else this
    /// field, else the executable's own file stem — so a distributed app binary namespaces under
    /// its own name out of the box, and the toolchain passes the project's package name. `None`
    /// here just means "fall through to the env/exe-stem default". `$NOETA_P2P_DIR` (an absolute
    /// path) overrides everything, for a caller that wants one exact location.
    pub app_id: Option<String>,
    /// If `false`, ignore any data dir and run fully ephemeral (fresh identity, in-memory store) —
    /// the pre-P3.3 behavior, kept for tests and throwaway nodes.
    pub persist: bool,
}

impl Default for P2pConfig {
    /// Persistent by default: a `noeta run` of a p2p program keeps its identity and synced logs
    /// across restarts with zero configuration (p2p P3.3).
    fn default() -> Self {
        P2pConfig::persistent()
    }
}

impl P2pConfig {
    /// The default persistent config: per-app XDG data dir, state persisted across restarts.
    pub fn persistent() -> P2pConfig {
        P2pConfig {
            data_dir: None,
            app_id: None,
            persist: true,
        }
    }

    /// A fully ephemeral node: fresh identity, in-memory store, no disk touched.
    pub fn ephemeral() -> P2pConfig {
        P2pConfig {
            data_dir: None,
            app_id: None,
            persist: false,
        }
    }

    /// Persist into an explicit directory (created if absent). Used by the two-node integration
    /// tests to give each in-process replica its own identity + store.
    pub fn at(dir: impl Into<PathBuf>) -> P2pConfig {
        P2pConfig {
            data_dir: Some(dir.into()),
            app_id: None,
            persist: true,
        }
    }

    /// Set the application namespace ([`Self::app_id`]) the default data dir keys on.
    pub fn with_app(mut self, app_id: Option<String>) -> P2pConfig {
        self.app_id = app_id;
        self
    }
}

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
    /// endpoint's key, so a peer attributes received operations to this author. Loaded from disk
    /// (persisted across restarts, p2p P3.3) or freshly generated for an ephemeral node.
    signing_key: SigningKey,
    /// The append-only operation log store (in-memory SQLite). Holds this node's log and peers'
    /// synced logs, giving a late-joining replica the full history to converge from.
    store: SqliteStore,
    /// The log-sync engine (over `store` + the endpoint + gossip). Joins a topic via `stream`.
    log_sync: LogSync<SqliteStore, LogId, ()>,
    /// topic → its durable state: the sync handle (join), plus this author's log tip (seq + backlink)
    /// so appends stay a correct append-only chain.
    durable: Mutex<HashMap<String, DurableTopic>>,
    /// topic → its live [`SyncStatus`] (p2p P3.3), shared with the drain task that updates it from
    /// the log-sync session lifecycle. `Arc` because the task outlives the borrow that spawns it.
    status: Arc<Mutex<HashMap<String, SyncStatus>>>,

    // --- Group encryption (p2p P3.4b), backing an encrypted synced_signal ---
    /// Where persistent state lives (identity, store, and the encryption credentials/store), or
    /// `None` for an ephemeral node. Kept so the lazily-built [`CryptoGroups`] can find its files.
    data_dir: Option<PathBuf>,
    /// This node's group-encryption component, built lazily on first encrypted use and cached (a
    /// plaintext-only program never constructs it). Its identity is the node's [`signing_key`], so
    /// encryption membership is keyed on the same peer id as the transport.
    crypto: Mutex<Option<CryptoGroups>>,
    /// topic → the encrypted group running on it (its [`EncryptedGroup`] state machine, durable
    /// subscription, and a buffer of decrypted payloads not yet returned to the program).
    groups: Mutex<HashMap<String, GroupEntry>>,
    /// group subscription id → its topic, so [`Self::group_poll`] finds the group by the id the
    /// program holds.
    group_subs: Mutex<HashMap<u64, String>>,
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
            .field(
                "subscriptions",
                &self.subs.lock().map(|s| s.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl P2pNode {
    /// Build and start a node with the default (persistent) config — see [`Self::start_with_config`].
    pub fn start() -> Result<P2pNode, StdError> {
        Self::start_with_config(P2pConfig::default())
    }

    /// Build and start the node (blocking until its components are up) with an explicit config.
    /// Fails only if the runtime or the core networking components (endpoint, gossip) cannot start;
    /// mDNS failure is tolerated.
    ///
    /// Identity & store follow `config` (p2p P3.3): a persistent node loads its Ed25519 key and an
    /// on-disk SQLite log store from `config`'s data dir (created on first run), so it restarts with
    /// the same identity and its prior synced state; an ephemeral node gets a fresh key and an
    /// in-memory store.
    pub fn start_with_config(config: P2pConfig) -> Result<P2pNode, StdError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| io_error(format!("cannot start the p2p node runtime: {e}")))?;

        // Resolve where (if anywhere) persistent state lives. A persistent node with a usable data
        // dir loads/saves its identity there and uses an on-disk store; otherwise it is ephemeral.
        let data_dir = if config.persist {
            match config
                .data_dir
                .clone()
                .or_else(|| default_data_dir(config.app_id.as_deref()))
            {
                Some(dir) => match ensure_dir(&dir) {
                    Ok(()) => Some(dir),
                    Err(e) => {
                        eprintln!(
                            "noeta p2p: cannot use data dir {} ({e}); running ephemeral (no persisted identity/state)",
                            dir.display()
                        );
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };

        // This node's identity — loaded from disk (stable across restarts, p2p P3.3) or freshly
        // generated. It signs the endpoint AND every log operation, so peers attribute synced
        // operations to this author.
        let signing_key = match &data_dir {
            Some(dir) => load_or_create_identity(&dir.join("identity.key")),
            None => SigningKey::generate(),
        };
        // The durable store: an on-disk SQLite log (survives restart) when persisting, else the
        // in-memory store (P3.2 behavior).
        let store_url = data_dir
            .as_ref()
            .map(|dir| format!("sqlite://{}", dir.join("store.db").display()));

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
            // The durable transport: an append-log store + the log-sync engine over the same
            // endpoint/gossip. `synced_signal` publishes/subscribes through this. On-disk (survives
            // restart, p2p P3.3) when persisting, else in-memory (P3.2).
            let store = match &store_url {
                Some(url) => SqliteStoreBuilder::new().database_url(url).build().await,
                None => SqliteStoreBuilder::memory().build().await,
            }
            .map_err(|e| io_error(format!("p2p store: {e}")))?;
            let log_sync =
                LogSync::<_, LogId, _>::builder(store.clone(), endpoint.clone(), gossip.clone())
                    .spawn()
                    .await
                    .map_err(|e| io_error(format!("p2p log-sync: {e}")))?;
            Ok::<_, StdError>((
                address_book,
                endpoint,
                discovery,
                mdns,
                gossip,
                store,
                log_sync,
            ))
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
            status: Arc::new(Mutex::new(HashMap::new())),
            data_dir,
            crypto: Mutex::new(None),
            groups: Mutex::new(HashMap::new()),
            group_subs: Mutex::new(HashMap::new()),
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
                .associate(
                    &Self::topic_of(topic),
                    &self.signing_key.verifying_key(),
                    &LOG_ID,
                )
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
        // Joined but not yet synced with any peer — Offline until a session reaches this topic.
        self.status
            .lock()
            .unwrap()
            .entry(topic.to_string())
            .or_insert(SyncStatus::Offline);
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
            let status = Arc::clone(&self.status);
            let topic_owned = topic.to_string();
            self.runtime.spawn(async move {
                while let Some(Ok(from_sync)) = stream.next().await {
                    match from_sync.event {
                        // A new operation carries a payload to merge. It also implies we are (at
                        // least) syncing with a peer, so keep the status at Synced/Syncing, never
                        // regressing it to Offline mid-stream.
                        TopicLogSyncEvent::OperationReceived { operation, .. } => {
                            if let Some(body) = operation.body
                                && tx.send(body.to_bytes()).is_err()
                            {
                                break; // receiver gone
                            }
                        }
                        // Session lifecycle → SyncStatus (p2p P3.3). A session is opening/replaying
                        // (Syncing) until all past state is replicated (SyncFinished) or we go live
                        // (LiveModeStarted), at which point we are caught up (Synced). A finished or
                        // failed session with no live successor leaves us Offline until the next.
                        TopicLogSyncEvent::SessionStarted
                        | TopicLogSyncEvent::SyncStarted { .. } => {
                            set_status(&status, &topic_owned, SyncStatus::Syncing);
                        }
                        TopicLogSyncEvent::SyncFinished { .. }
                        | TopicLogSyncEvent::LiveModeStarted => {
                            set_status(&status, &topic_owned, SyncStatus::Synced);
                        }
                        TopicLogSyncEvent::SessionFinished { .. }
                        | TopicLogSyncEvent::Failed { .. } => {
                            set_status(&status, &topic_owned, SyncStatus::Offline);
                        }
                    }
                }
            });
        }
        let id = self.next_sub.fetch_add(1, Ordering::Relaxed);
        self.subs.lock().unwrap().insert(id, rx);
        Ok(id)
    }

    // --- Identity & status (p2p P3.3) ---------------------------------------------------------

    /// This node's stable identity: the hex-encoded Ed25519 public key it signs operations with
    /// (persisted, so it is the same across restarts of a persistent node).
    pub fn identity(&self) -> String {
        self.signing_key.verifying_key().to_hex()
    }

    /// The current [`SyncStatus`] for `topic` — [`SyncStatus::Offline`] for a topic never joined or
    /// with no live peer, updated by the drain task from the log-sync session lifecycle.
    pub fn sync_status(&self, topic: &str) -> SyncStatus {
        self.status
            .lock()
            .unwrap()
            .get(topic)
            .copied()
            .unwrap_or(SyncStatus::Offline)
    }

    // --- Group encryption (p2p P3.4b) ---------------------------------------------------------

    /// This node's group-encryption component, built on first use and cached. The encryption actor
    /// is the node's own signing key (loaded via persisted [`Credentials`] whose signing key is the
    /// node identity), so membership is keyed on the same peer id as the transport. A persistent
    /// node keeps its encryption store (`spaces.db`) and credentials (`credentials.key`) in its data
    /// dir; an ephemeral node builds both in memory.
    fn crypto(&self) -> Result<CryptoGroups, StdError> {
        let mut guard = self.crypto.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        let rng = Rng::default();
        let credentials = match &self.data_dir {
            Some(dir) => {
                load_or_create_credentials(&dir.join("credentials.key"), &self.signing_key, &rng)
            }
            None => {
                let identity_secret = SecretKey::from_rng(&rng)
                    .map_err(|e| io_error(format!("cannot generate encryption identity: {e}")))?;
                Credentials::from_keys(self.signing_key.clone(), identity_secret)
            }
        };
        let spaces_url = self
            .data_dir
            .as_ref()
            .map(|dir| format!("sqlite://{}", dir.join("spaces.db").display()));
        let store = self
            .runtime
            .block_on(async {
                match &spaces_url {
                    Some(url) => SqliteStoreBuilder::new().database_url(url).build().await,
                    None => SqliteStoreBuilder::memory().build().await,
                }
            })
            .map_err(|e| io_error(format!("cannot open group-encryption store: {e}")))?;
        let crypto = CryptoGroups::new(store, credentials, rng)?;
        *guard = Some(crypto.clone());
        Ok(crypto)
    }

    /// The hex actor id of this node's group-encryption identity. Equal to [`Self::identity`] — the
    /// encryption group and the transport share one peer id.
    pub fn crypto_group_id(&self) -> Result<String, StdError> {
        Ok(self.crypto()?.id().to_hex())
    }

    /// Open an encrypted group on `topic` for `members` (peer-id hex strings), returning a
    /// subscription id polled through [`Self::group_poll`]. Subscribes to the durable transport,
    /// then broadcasts this node's key bundle (and, if it is the elected creator, the space-creation
    /// operations) so members can converge — the model-A handshake driven over the real transport.
    pub fn group_open(&self, topic: &str, members: &[String]) -> Result<u64, StdError> {
        let members = parse_members(members)?;
        let crypto = self.crypto()?;
        // The async crypto handshake runs in one `block_on` producing wire ops; the transport
        // publishes (each its own `block_on`) happen *after*, never nested inside it.
        let (group, to_send) = self
            .runtime
            .block_on(EncryptedGroup::open(crypto, topic, members))?;
        let sub = self.subscribe_durable(topic)?;
        for op in to_send {
            self.publish_durable(topic, op)?;
        }
        self.groups.lock().unwrap().insert(
            topic.to_string(),
            GroupEntry {
                group,
                inbox: VecDeque::new(),
            },
        );
        self.group_subs
            .lock()
            .unwrap()
            .insert(sub, topic.to_string());
        Ok(sub)
    }

    /// Publish `plaintext` to the encrypted group on `topic`: encrypt it to the members and broadcast
    /// (or buffer it until this node is welcomed, then flush).
    pub fn group_publish(&self, topic: &str, plaintext: Vec<u8>) -> Result<(), StdError> {
        let to_send = {
            let mut groups = self.groups.lock().unwrap();
            let entry = groups
                .get_mut(topic)
                .ok_or_else(|| io_error(format!("no encrypted group open on `{topic}`")))?;
            self.runtime.block_on(entry.group.publish(plaintext))?
        };
        for op in to_send {
            self.publish_durable(topic, op)?;
        }
        Ok(())
    }

    /// The next decrypted application payload for group subscription `sub`, or `None`. Drains control
    /// operations (key bundles, welcomes) as a side effect — broadcasting any welcome ops the creator
    /// produces — and buffers decrypted payloads, returning them one per call.
    pub fn group_poll(&self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        let topic = match self.group_subs.lock().unwrap().get(&sub) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        loop {
            // Return a buffered decrypted payload first, if any.
            {
                let mut groups = self.groups.lock().unwrap();
                if let Some(entry) = groups.get_mut(&topic)
                    && let Some(payload) = entry.inbox.pop_front()
                {
                    return Ok(Some(payload));
                }
            }
            // Otherwise pull the next raw operation off the transport and process it.
            let Some(raw) = self.poll_sub(sub) else {
                return Ok(None);
            };
            let to_send = {
                let mut groups = self.groups.lock().unwrap();
                let entry = groups
                    .get_mut(&topic)
                    .ok_or_else(|| io_error(format!("no encrypted group open on `{topic}`")))?;
                let out = self.runtime.block_on(entry.group.receive(&raw))?;
                entry.inbox.extend(out.decrypted);
                out.to_send
            };
            for op in to_send {
                self.publish_durable(&topic, op)?;
            }
        }
    }

    /// Add `member` (peer-id hex) to the encrypted group on `topic` at runtime, broadcasting any
    /// welcome operations. Only the group creator manages membership; a no-op elsewhere.
    pub fn group_add(&self, topic: &str, member: &str) -> Result<(), StdError> {
        let member = VerifyingKey::from_str(member)
            .map_err(|e| io_error(format!("invalid peer id `{member}`: {e}")))?;
        let to_send = {
            let mut groups = self.groups.lock().unwrap();
            let entry = groups
                .get_mut(topic)
                .ok_or_else(|| io_error(format!("no encrypted group open on `{topic}`")))?;
            self.runtime.block_on(entry.group.add_member(member))?
        };
        for op in to_send {
            self.publish_durable(topic, op)?;
        }
        Ok(())
    }

    /// Remove `member` (peer-id hex) from the encrypted group on `topic` at runtime, **rotating the
    /// group key** so it can no longer decrypt new state, and broadcasting the removal operations.
    /// Only the group creator manages membership; a no-op elsewhere.
    pub fn group_remove(&self, topic: &str, member: &str) -> Result<(), StdError> {
        let member = VerifyingKey::from_str(member)
            .map_err(|e| io_error(format!("invalid peer id `{member}`: {e}")))?;
        let to_send = {
            let mut groups = self.groups.lock().unwrap();
            let entry = groups
                .get_mut(topic)
                .ok_or_else(|| io_error(format!("no encrypted group open on `{topic}`")))?;
            self.runtime.block_on(entry.group.remove_member(member))?
        };
        for op in to_send {
            self.publish_durable(topic, op)?;
        }
        Ok(())
    }
}

/// One topic's encrypted group: its [`EncryptedGroup`] state machine plus a buffer of decrypted
/// payloads not yet handed back to the program (one `group_poll` may decrypt several at once — e.g.
/// a welcome that releases buffered messages).
struct GroupEntry {
    group: EncryptedGroup,
    inbox: VecDeque<Vec<u8>>,
}

/// Parse a member set of peer-id hex strings into actor ids.
fn parse_members(members: &[String]) -> Result<Vec<ActorId>, StdError> {
    members
        .iter()
        .map(|m| {
            VerifyingKey::from_str(m)
                .map_err(|e| io_error(format!("invalid peer id `{m}` in members: {e}")))
        })
        .collect()
}

/// Set (overwrite) a topic's [`SyncStatus`], used by the drain task as session events arrive.
fn set_status(status: &Arc<Mutex<HashMap<String, SyncStatus>>>, topic: &str, next: SyncStatus) {
    status.lock().unwrap().insert(topic.to_string(), next);
}

/// The per-user default p2p data dir for this application. `$NOETA_P2P_DIR` (an absolute path)
/// short-circuits everything; otherwise it is `<XDG data dir>/<app>/p2p`, where `<app>` is the
/// resolved application namespace ([`resolve_app`]) — so **two different Noeta apps never share one
/// identity/store dir** and clobber each other (the whole point of keying on the app). Persistent
/// state, so the XDG *data* dir, not the cache dir. `None` when no home can be resolved (a bare
/// CI/container), in which case the node runs ephemeral.
fn default_data_dir(app_id: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = env_path("NOETA_P2P_DIR") {
        return Some(dir);
    }
    let app = resolve_app(app_id);
    Some(xdg_data_root()?.join(app).join("p2p"))
}

/// The application namespace the default data dir is keyed on: `$NOETA_P2P_APP` if set (the explicit
/// escape hatch, honored in any deployment), else the caller-supplied `app_id` (the toolchain passes
/// the project's package name), else the running executable's own file stem — so a distributed app
/// binary namespaces under its own name with no configuration, while the shared `noeta` toolchain
/// gets a per-project id from the caller. Sanitized so a `company/package` name is a single safe
/// path segment. Falls back to `"noeta"` only when nothing else is knowable.
fn resolve_app(app_id: Option<&str>) -> String {
    let raw = std::env::var("NOETA_P2P_APP")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| app_id.map(str::to_string))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
        })
        .unwrap_or_else(|| "noeta".to_string());
    sanitize_segment(&raw)
}

/// Reduce an app id to one filesystem-safe path segment: keep alphanumerics, `.`, `-`, `_`; map
/// every other byte (path separators, spaces, the `/` in `company/package`) to `-`. Non-empty.
fn sanitize_segment(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("noeta");
    }
    out
}

/// The per-user XDG *data* root (persistent app state, not cache), OS-appropriate.
fn xdg_data_root() -> Option<PathBuf> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = env_path("XDG_DATA_HOME") {
            return Some(xdg);
        }
        Some(home()?.join(".local").join("share"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(home()?.join("Library").join("Application Support"))
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = env_path("LOCALAPPDATA") {
            return Some(local);
        }
        Some(home()?.join("AppData").join("Local"))
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env_path("USERPROFILE")
    }
    #[cfg(not(windows))]
    {
        env_path("HOME")
    }
}

/// Create the data dir (and parents) if absent, private (`0700`) on Unix so the identity key is not
/// world-readable.
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Load this node's Ed25519 identity from `path`, or generate + persist a fresh one there. Any read
/// error, wrong length, or write failure falls back to an ephemeral (unsaved) key with a warning —
/// the node still runs, just without a stable identity this session.
fn load_or_create_identity(path: &Path) -> SigningKey {
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return SigningKey::from_bytes(&arr);
        }
        eprintln!(
            "noeta p2p: identity file {} is malformed; generating a new identity",
            path.display()
        );
    }
    let key = SigningKey::generate();
    if let Err(e) = write_private(path, key.as_bytes()) {
        eprintln!(
            "noeta p2p: cannot persist identity to {} ({e}); identity is ephemeral this session",
            path.display()
        );
    }
    key
}

/// Load this node's group-encryption [`Credentials`] from `path`, or derive + persist fresh ones.
/// The signing key is always the node's transport identity (`signing_key`), so the encryption actor
/// id equals the node's peer id; the paired x25519 identity secret (needed for key agreement, and
/// which the crypto library will not expose as raw bytes) is generated once and the whole
/// `Credentials` persisted via serde. A file whose identity no longer matches, or is unreadable, is
/// regenerated with a warning — the node still runs, just with a fresh (this-session) encryption
/// secret.
fn load_or_create_credentials(path: &Path, signing_key: &SigningKey, rng: &Rng) -> Credentials {
    if let Ok(bytes) = std::fs::read(path) {
        match postcard::from_bytes::<Credentials>(&bytes) {
            Ok(creds) if creds.verifying_key() == signing_key.verifying_key() => return creds,
            Ok(_) => eprintln!(
                "noeta p2p: credentials file {} does not match the node identity; regenerating",
                path.display()
            ),
            Err(_) => eprintln!(
                "noeta p2p: credentials file {} is malformed; regenerating",
                path.display()
            ),
        }
    }
    let identity_secret =
        SecretKey::from_rng(rng).expect("x25519 identity secret from the system rng");
    let credentials = Credentials::from_keys(signing_key.clone(), identity_secret);
    match postcard::to_allocvec(&credentials) {
        Ok(bytes) => {
            if let Err(e) = write_private(path, &bytes) {
                eprintln!(
                    "noeta p2p: cannot persist credentials to {} ({e}); encryption identity is ephemeral this session",
                    path.display()
                );
            }
        }
        Err(e) => eprintln!("noeta p2p: cannot serialize credentials ({e}); not persisting"),
    }
    credentials
}

/// Write `bytes` to `path`, owner-only (`0600`) on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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

/// The real node **is** a [`P2p`](noeta_ext_abi::host::P2p) backend (para-namespace F2b): the
/// `para.p2p` extension owns one of these in ctx state and reaches it through the same seam as the
/// loopback broker, so the transport lives entirely on the extension side — no host implements
/// `P2p`. Every method delegates to the node's inherent operation (all `&self`: the node is
/// internally synchronized; the trait's `&mut self` composes over that). The trait's own defaults —
/// which the loopback broker relies on — are all overridden here with the real log-sync / spaces /
/// identity behaviour, exactly as `RealHost` used to.
impl noeta_ext_abi::host::P2p for P2pNode {
    fn p2p_publish(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.publish(topic, message)
    }

    fn p2p_poll(&mut self, topic: &str) -> Result<Option<Vec<u8>>, StdError> {
        self.poll_default(topic)
    }

    fn p2p_subscribe(&mut self, topic: &str) -> Result<u64, StdError> {
        self.subscribe(topic)
    }

    fn p2p_poll_sub(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        Ok(self.poll_sub(sub))
    }

    fn p2p_publish_durable(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.publish_durable(topic, message)
    }

    fn p2p_subscribe_durable(&mut self, topic: &str) -> Result<u64, StdError> {
        self.subscribe_durable(topic)
    }

    fn p2p_group_open(&mut self, topic: &str, members: &[String]) -> Result<u64, StdError> {
        self.group_open(topic, members)
    }

    fn p2p_group_publish(&mut self, topic: &str, plaintext: Vec<u8>) -> Result<(), StdError> {
        self.group_publish(topic, plaintext)
    }

    fn p2p_group_poll(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        self.group_poll(sub)
    }

    fn p2p_group_add(&mut self, topic: &str, member: &str) -> Result<(), StdError> {
        self.group_add(topic, member)
    }

    fn p2p_group_remove(&mut self, topic: &str, member: &str) -> Result<(), StdError> {
        self.group_remove(topic, member)
    }

    fn p2p_identity(&mut self) -> Result<Option<String>, StdError> {
        Ok(Some(self.identity()))
    }

    fn p2p_sync_status(&mut self, topic: &str) -> SyncStatus {
        self.sync_status(topic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node boots and its gossip pipeline runs: join a topic, publish (to no one), subscribe,
    /// and confirm a non-blocking poll is empty. Real cross-node delivery is exercised by the
    /// two-node integration test (P3.1); this pins that the async bridge itself works.
    /// Two real nodes on the same topic (discovered over mDNS) exchange a gossip message. Not
    /// hermetic — needs real multicast/networking — so `#[ignore]`, run explicitly:
    /// `cargo test -p noeta-host-real --features ring-p2p -- --ignored two_nodes`.
    /// The durable (log-sync) catch-up guarantee that gossip lacks: node A publishes **before**
    /// node B exists, and B still receives it once it joins and syncs A's log. Not hermetic (real
    /// networking) — run explicitly:
    /// `cargo test -p noeta-host-real --features ring-p2p -- --ignored durable_catch_up`.
    /// A unique throwaway data dir under the OS temp dir, removed on drop — so a persistence test
    /// exercises the real on-disk identity/store path without touching the user's XDG dir.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("noeta-p2p-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The node **is** a [`noeta_ext_abi::host::P2p`] backend (para-namespace F2b) — the extension owns
    /// one and drives it through the trait. Single node, so no delivery; this pins that the trait
    /// delegation + lazy start work end to end (identity present, status Offline with no peer).
    /// Starting a node binds local sockets only, so it stays hermetic (unlike the two-node tests).
    #[test]
    fn a_node_serves_the_p2p_trait() {
        use noeta_ext_abi::{P2p, SyncStatus};
        let dir = TempDir::new("trait");
        let mut node = P2pNode::start_with_config(P2pConfig::at(&dir.0)).expect("node starts");
        node.p2p_publish("room", b"hello".to_vec())
            .expect("publish via the trait");
        let sub = node.p2p_subscribe("room").expect("subscribe via the trait");
        assert_eq!(node.p2p_poll_sub(sub).expect("poll_sub"), None);
        assert_eq!(node.p2p_poll("other").expect("poll"), None);
        assert!(node.p2p_identity().expect("identity").is_some());
        assert_eq!(node.p2p_sync_status("room"), SyncStatus::Offline);
    }

    #[test]
    #[ignore = "needs real networking (mDNS multicast); run explicitly"]
    fn durable_catch_up_reaches_a_late_joiner() {
        // Distinct ephemeral nodes (fresh identity + in-memory store each): A and B are two real
        // replicas, so B must catch up on A's log rather than already share A's identity/state.
        let a = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node a");
        // A subscribes (joins the overlay) and publishes durably — this goes into A's log.
        let _sub_a = a.subscribe_durable("room").expect("a subscribes");
        a.publish_durable("room", b"durable state".to_vec())
            .expect("a publishes durably");
        std::thread::sleep(std::time::Duration::from_secs(2));

        // B starts LATE — after A already published — and subscribes. Over gossip it would miss
        // the message; over log-sync it syncs A's log and catches up.
        let b = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node b");
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
        let a = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node a");
        let b = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node b");
        // Both subscribe first (gossip is ephemeral — only messages published after subscribing
        // arrive), then give discovery a moment to connect the overlay.
        let sub_b = b.subscribe("room").expect("b subscribes");
        let _sub_a = a.subscribe("room").expect("a subscribes");
        std::thread::sleep(std::time::Duration::from_secs(3));

        a.publish("room", b"hi from a".to_vec())
            .expect("a publishes");

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
        let node = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node starts");
        node.publish("room", b"hello".to_vec())
            .expect("publish to an empty overlay succeeds");
        let sub = node.subscribe("room").expect("subscribe succeeds");
        // No peers, so nothing is delivered — but the poll path must work and be empty.
        assert_eq!(node.poll_sub(sub), None);
        assert_eq!(node.poll_sub(999), None); // unknown subscription id
    }

    /// P3.3: a persistent node writes its Ed25519 identity to its data dir and, restarted against
    /// the same dir, comes back with the *same* identity — the offline-restart guarantee. Also
    /// asserts an ephemeral node's identity differs (a fresh key each time).
    #[test]
    fn identity_persists_across_restart() {
        let dir = TempDir::new("identity");

        let first = P2pNode::start_with_config(P2pConfig::at(&dir.0)).expect("node 1");
        let id1 = first.identity();
        drop(first); // release the store/socket before restarting against the same dir

        let second = P2pNode::start_with_config(P2pConfig::at(&dir.0)).expect("node 2");
        assert_eq!(
            second.identity(),
            id1,
            "restart reuses the persisted identity"
        );
        assert!(dir.0.join("identity.key").exists(), "identity file written");

        // A fresh ephemeral node has a different identity (nothing loaded).
        let ephemeral = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("ephemeral node");
        assert_ne!(ephemeral.identity(), id1);
    }

    /// P3.4: the default data dir is namespaced per application, so two different Noeta apps never
    /// share one identity/store dir. Skipped if an env override is in effect on the test host.
    #[test]
    fn default_data_dir_is_namespaced_per_app() {
        if std::env::var_os("NOETA_P2P_DIR").is_some()
            || std::env::var_os("NOETA_P2P_APP").is_some()
        {
            return; // an env override collapses the namespace; nothing to assert
        }
        if let (Some(a), Some(b)) = (
            default_data_dir(Some("acme/chat")),
            default_data_dir(Some("acme/wiki")),
        ) {
            assert_ne!(a, b, "different apps get different dirs");
            assert!(a.to_string_lossy().contains("acme-chat"));
            assert!(b.to_string_lossy().contains("acme-wiki"));
            assert!(a.ends_with("p2p"));
        }
    }

    /// P3.4b: the group-encryption identity is the node's own identity (same peer id for membership
    /// and transport), and — like the transport identity — it persists across a restart against the
    /// same data dir (the x25519 secret is regenerated only when the file is absent/mismatched).
    #[test]
    fn crypto_identity_matches_node_identity_and_persists() {
        let dir = TempDir::new("crypto-id");

        let (node_id, group_id) = {
            let node = P2pNode::start_with_config(P2pConfig::at(&dir.0)).expect("node 1");
            (node.identity(), node.crypto_group_id().expect("group id"))
        };
        assert_eq!(
            group_id, node_id,
            "the encryption actor id equals the node's transport identity"
        );
        assert!(
            dir.0.join("credentials.key").exists(),
            "credentials written"
        );

        // Restart from the same dir: the credentials (and thus the group actor id) persist.
        let second = P2pNode::start_with_config(P2pConfig::at(&dir.0)).expect("node 2");
        assert_eq!(
            second.crypto_group_id().expect("group id"),
            group_id,
            "credentials persist across restart"
        );

        // A fresh ephemeral node gets a different encryption identity.
        let ephemeral = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("ephemeral");
        assert_ne!(ephemeral.crypto_group_id().expect("group id"), group_id);
    }

    /// P3.4b.3: two **real** nodes converge on **encrypted** state over the live transport. Both
    /// open the same encrypted group (the member set is the two node ids); the elected creator
    /// welcomes the other as key bundles flow, then publishes a secret that the other decrypts —
    /// the QUIC-over-iroh counterpart of the hermetic `EncryptedGroup` relay test. Not hermetic
    /// (real mDNS multicast + networking), so `#[ignore]`; run explicitly:
    /// `cargo test -p noeta-host-real --features ring-p2p -- --ignored two_nodes_converge_encrypted`.
    #[test]
    #[ignore = "needs real networking (mDNS multicast); run explicitly"]
    fn two_nodes_converge_on_encrypted_state() {
        let a = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node a");
        let b = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node b");
        let members = vec![
            a.crypto_group_id().expect("a id"),
            b.crypto_group_id().expect("b id"),
        ];
        let a_is_creator = a.crypto_group_id().unwrap() == *members.iter().min().unwrap();

        let sub_a = a
            .group_open("secure/room", &members)
            .expect("a opens group");
        let sub_b = b
            .group_open("secure/room", &members)
            .expect("b opens group");
        // Give discovery + the key-bundle/welcome handshake time to flow; polling both drives it
        // (each poll processes received ops and broadcasts any welcomes).
        for _ in 0..50 {
            let _ = a.group_poll(sub_a);
            let _ = b.group_poll(sub_b);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // The creator publishes (it is welcomed by construction); the other polls until it decrypts.
        let secret = b"converged encrypted secret".to_vec();
        let (publisher, reader, reader_sub) = if a_is_creator {
            (&a, &b, sub_b)
        } else {
            (&b, &a, sub_a)
        };
        publisher
            .group_publish("secure/room", secret.clone())
            .expect("creator publishes");

        let mut received = None;
        for _ in 0..150 {
            // Poll the publisher too, so it keeps servicing the handshake.
            let _ = publisher.group_poll(if a_is_creator { sub_a } else { sub_b });
            if let Some(bytes) = reader.group_poll(reader_sub).expect("reader polls") {
                received = Some(bytes);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(received.as_deref(), Some(&secret[..]));
    }

    /// P3.4b dynamic membership over **real** networking: the creator removes the other member and
    /// publishes state the removed member can no longer decrypt (revocation via key rotation). The
    /// QUIC counterpart of the hermetic `removed_member_cannot_decrypt_new_state` test. `#[ignore]`
    /// (needs real mDNS); run explicitly.
    #[test]
    #[ignore = "needs real networking (mDNS multicast); run explicitly"]
    fn two_nodes_revocation_over_the_wire() {
        let a = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node a");
        let b = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node b");
        let a_id = a.crypto_group_id().unwrap();
        let b_id = b.crypto_group_id().unwrap();
        let members = vec![a_id.clone(), b_id.clone()];
        let a_is_creator = a_id == *members.iter().min().unwrap();
        // The creator manages membership; it removes the other member.
        let (creator, creator_sub, other, other_sub, removed_id) = {
            let sub_a = a.group_open("secure/rev", &members).expect("a opens");
            let sub_b = b.group_open("secure/rev", &members).expect("b opens");
            if a_is_creator {
                (&a, sub_a, &b, sub_b, b_id)
            } else {
                (&b, sub_b, &a, sub_a, a_id)
            }
        };
        // Settle the handshake.
        for _ in 0..50 {
            let _ = creator.group_poll(creator_sub);
            let _ = other.group_poll(other_sub);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Remove the other member (rotates the key), then publish a secret it must not decrypt.
        creator
            .group_remove("secure/rev", &removed_id)
            .expect("creator removes member");
        let secret = b"post-revocation secret".to_vec();
        creator
            .group_publish("secure/rev", secret.clone())
            .expect("creator publishes");

        let mut leaked = false;
        for _ in 0..100 {
            let _ = creator.group_poll(creator_sub);
            if let Some(bytes) = other.group_poll(other_sub).expect("other polls")
                && bytes == secret
            {
                leaked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            !leaked,
            "a removed member must not decrypt post-revocation state"
        );
    }

    #[test]
    fn sanitize_maps_unsafe_bytes_to_one_segment() {
        assert_eq!(sanitize_segment("acme/chat"), "acme-chat");
        assert_eq!(sanitize_segment("a b/c"), "a-b-c");
        assert_eq!(sanitize_segment("ok_name.1-2"), "ok_name.1-2");
        assert_eq!(sanitize_segment(""), "noeta");
    }

    /// P3.3: with no peer reachable, every topic reports `Offline` — an unjoined topic trivially,
    /// and even a joined-but-unpartnered `synced_signal` topic, since no sync session ever starts.
    #[test]
    fn sync_status_is_offline_without_a_peer() {
        let node = P2pNode::start_with_config(P2pConfig::ephemeral()).expect("node starts");
        assert_eq!(node.sync_status("never-joined"), SyncStatus::Offline);
        let _sub = node.subscribe_durable("room").expect("subscribe");
        // Joined the topic, but no peer exists to sync with → still Offline.
        assert_eq!(node.sync_status("room"), SyncStatus::Offline);
    }
}
