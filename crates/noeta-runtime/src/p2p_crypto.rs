//! Group encryption for `synced_signal` (p2p P3.4b) — the p2panda-spaces assembly.
//!
//! `synced_signal`'s bytes cross the wire in the clear on the P3.2 durable transport. A *group*
//! `synced_signal` instead encrypts every state it publishes to a **space**: a p2panda-spaces
//! `Space` is an auth-controlled membership group ([`p2panda_auth`]) with an encryption context
//! ([`p2panda_encryption`]'s symmetric "data encryption" scheme — XChaCha20-Poly1305, so a member
//! that joins late still decrypts prior state, exactly what a convergent CRDT needs). We do not
//! hand-roll crypto; we assemble p2panda's pieces:
//!
//! - a **[`NoetaManager`]** owns the group + encryption state, backed by a [`NoetaSpacesStore`] (the
//!   six storage traits it needs, provided by `p2panda-store`'s `spaces` feature);
//! - a **[`NoetaForge`]** mints the signed operations that carry spaces control/data messages (their
//!   [`SpacesArgs`] ride as the operation header's extensions), persisting them to the log store;
//! - **[`SpacesOp`]** is the message newtype: `Forge::Message` must be `Borrow<SpacesArgs>`, and the
//!   orphan rule blocks impl'ing that on the foreign `Operation` directly, so we wrap it.
//!
//! [`CryptoGroups`] is the production component: the manager is **store-backed and stateless between
//! calls** — every mutating call (`create_space`/`publish`/`add`/`process`) returns fresh auth/space
//! state that the caller must write back before the next call reads it. p2panda-spaces gates its own
//! state setters behind `test_utils`, and `p2panda-stream`'s spaces processor stubs persistence as a
//! `@TODO`, so we persist through the **public store traits** directly ([`persist_groups`] /
//! [`persist_space`], mirroring the crate's test-only `*_persisted` wrappers). This works in a
//! shipping build with no test-only features. Received operations are fed to [`NoetaManager::process`]
//! in causal order; decrypted application data surfaces as [`Event::Application`].
//!
//! Binding [`CryptoGroups`] to the real node transport (encrypting `synced_signal` bodies over
//! log-sync) is the next step (P3.4b.1).

use std::borrow::Borrow;

use p2panda_auth::Access;
use p2panda_auth::group::GroupCrdtState;
use p2panda_core::traits::{Digest, Provenance};
use p2panda_core::{Hash, Header, Operation, SigningKey, VerifyingKey};
use p2panda_encryption::Rng;
use p2panda_spaces::space::SpacesState;
use p2panda_spaces::{ActorId, AuthMessage, Credentials, Event, Forge, SpaceId, SpacesArgs};
use p2panda_store::groups::GroupsStore;
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::spaces::{SpacesStore, SqliteSpacesStore};
use p2panda_store::{SqliteError, SqliteStore, Transaction, tx};

use crate::io_error;
use noeta_stdlib::StdError;

/// We don't use conditional access, so the spaces "conditions" type is unit.
pub type Conditions = ();
/// Our operations carry the spaces control/data args as their header extensions.
pub type SpacesExtensions = SpacesArgs<Conditions>;
type SpacesOperation = Operation<SpacesExtensions>;

/// The concrete auth-CRDT state type the manager returns and we persist. p2panda-spaces aliases this
/// as its private `AuthGroupState<C>`; we spell out the identical `p2panda_auth` alias so a
/// persistence helper can name it (the private alias is unreachable, the underlying type is public).
type AuthGroupState = GroupCrdtState<VerifyingKey, Hash, AuthMessage<Conditions>, Conditions>;

/// Every node keeps a single append-only log for its own spaces operations.
const SPACES_LOG_ID: u32 = 0;

/// The p2panda-spaces groups-context id under which the global auth CRDT state is stored. p2panda-
/// spaces keeps this private (its `GLOBAL_GROUPS_CONTEXT_ID`); we mirror the exact bytes because the
/// manager reads groups state back under `Hash::digest(..)` of this id, so our production
/// persistence must write it under the identical key or the manager can't find what we stored.
const GLOBAL_GROUPS_CONTEXT_ID: &[u8] = b"global-groups-context";

/// A p2panda operation carrying spaces args, wrapped so we can satisfy the `Forge::Message` bounds
/// (`Borrow<SpacesArgs>` — the orphan rule blocks impl'ing it on the foreign `Operation` directly).
#[derive(Debug, Clone)]
pub struct SpacesOp(pub SpacesOperation);

impl Borrow<SpacesExtensions> for SpacesOp {
    fn borrow(&self) -> &SpacesExtensions {
        &self.0.header.extensions
    }
}

impl Provenance<VerifyingKey> for SpacesOp {
    fn author(&self) -> VerifyingKey {
        self.0.header.verifying_key
    }

    fn verify(&self) -> bool {
        // Delegate to the wrapped operation's own signature verification.
        Provenance::verify(&self.0)
    }
}

impl Digest<Hash> for SpacesOp {
    fn hash(&self) -> Hash {
        self.0.hash
    }
}

impl SpacesOp {
    /// Serialize this operation for the transport. Spaces operations carry all their data — control
    /// state *and* the encrypted application ciphertext — in the header extensions ([`SpacesArgs`]),
    /// and [`NoetaForge`] always mints them with an empty body, so the canonical CBOR header
    /// encoding is the whole operation on the wire.
    pub fn to_wire(&self) -> Vec<u8> {
        self.0.header.to_bytes()
    }

    /// Reconstruct an operation received from the transport, recomputing its hash (the operation id)
    /// from the header bytes. The signature travels inside the header, so [`Provenance::verify`]
    /// still checks authenticity after a round-trip.
    pub fn from_wire(bytes: &[u8]) -> Result<SpacesOp, StdError> {
        let header: Header<SpacesExtensions> = p2panda_core::cbor::decode_cbor(bytes)
            .map_err(|e| io_error(format!("cannot decode spaces operation: {e}")))?;
        let hash = header.hash();
        Ok(SpacesOp(SpacesOperation {
            hash,
            header,
            body: None,
        }))
    }
}

/// Builds, signs and persists spaces operations for one node — the [`Forge`] p2panda-spaces calls to
/// mint control/data messages. Mirrors the shape of the crate's reference forge: the next entry's
/// seq/backlink come from this author's log in the store, and the forged operation is persisted.
#[derive(Debug, Clone)]
pub struct NoetaForge {
    signing_key: SigningKey,
    store: SqliteStore,
}

impl NoetaForge {
    pub fn new(store: SqliteStore, signing_key: SigningKey) -> NoetaForge {
        NoetaForge { signing_key, store }
    }
}

impl Forge<Conditions> for NoetaForge {
    type Message = SpacesOp;
    type Error = SqliteError;

    fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    async fn forge(&self, args: SpacesExtensions) -> Result<SpacesOp, SqliteError> {
        let operation = tx!(self.store, {
            let (seq_num, backlink) = <SqliteStore as LogStore<
                SpacesOperation,
                VerifyingKey,
                u32,
                u32,
                Hash,
            >>::get_latest_entry_tx(
                &self.store, &self.signing_key.verifying_key(), &SPACES_LOG_ID
            )
            .await?
            .map(|op| (op.header.seq_num + 1, Some(op.hash)))
            .unwrap_or((0, None));

            let mut header = Header {
                version: 1,
                verifying_key: self.signing_key.verifying_key(),
                signature: None,
                payload_size: 0,
                payload_hash: None,
                seq_num,
                backlink,
                extensions: args,
            };
            header.sign(&self.signing_key);
            let hash = header.hash();
            let operation = SpacesOperation {
                hash,
                header,
                body: None,
            };
            self.store
                .insert_operation(&hash, &operation, &SPACES_LOG_ID)
                .await?;
            operation
        });
        Ok(SpacesOp(operation))
    }
}

/// The spaces store type: the SQLite-backed store implementing the six traits [`NoetaManager`] needs.
pub type NoetaSpacesStore = SqliteSpacesStore<SpacesExtensions>;

/// The fully-applied `Manager` type for our group encryption.
pub type NoetaManager = p2panda_spaces::manager::Manager<
    NoetaSpacesStore,
    NoetaForge,
    Conditions,
    p2panda_spaces::StrongRemoveResolver<Conditions>,
>;

/// Persist the global auth state the manager returned. Mirrors p2panda-spaces' own (test-gated)
/// `Manager::set_groups_state`, but built on the public [`GroupsStore`] + [`Transaction`] traits so
/// it works in a shipping build. The context id must match the manager's private one (see
/// [`GLOBAL_GROUPS_CONTEXT_ID`]).
async fn persist_groups(
    store: &NoetaSpacesStore,
    groups_y: &AuthGroupState,
) -> Result<(), SqliteError> {
    let permit = store.begin().await?;
    store
        .set_groups_state_tx(Hash::digest(GLOBAL_GROUPS_CONTEXT_ID), groups_y)
        .await?;
    store.commit(permit).await?;
    Ok(())
}

/// Persist the space state the manager returned. Mirrors `Manager::set_space_state`, on the public
/// [`SpacesStore`] + [`Transaction`] traits.
async fn persist_space(
    store: &NoetaSpacesStore,
    space_y: SpacesState<Conditions>,
) -> Result<(), SqliteError> {
    let space_id = space_y.space_id;
    let state: p2panda_spaces::SpacesStoreState<Conditions> = space_y.into();
    let permit = store.begin().await?;
    store.set_space_state_tx(&space_id, &state).await?;
    store.commit(permit).await?;
    Ok(())
}

/// One node's group-encryption state (p2p P3.4b): a p2panda-spaces [`NoetaManager`] plus the store
/// handles to persist the state deltas each mutating call returns. The manager is store-backed and
/// stateless between calls, so this is the production analogue of the crate's test-only
/// `*_persisted` wrappers — built entirely on the public store traits, no test features.
///
/// Methods are async building blocks; the node (which owns the tokio runtime) drives them at the
/// synchronous host boundary.
#[derive(Clone)]
pub struct CryptoGroups {
    manager: NoetaManager,
    /// The base operation log — received operations are inserted here so the manager's dependency
    /// lookups resolve. Shares its SQLite pool with `spaces_store`.
    store: SqliteStore,
    /// The spaces/groups/space state store — the manager reads through it and we persist deltas to
    /// it. A clone of the same store handed to the manager (shared pool).
    spaces_store: NoetaSpacesStore,
}

impl std::fmt::Debug for CryptoGroups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CryptoGroups")
            .field("id", &self.manager.id())
            .finish_non_exhaustive()
    }
}

impl CryptoGroups {
    /// Build a group-encryption manager over `store`, using `credentials` (this actor's signing key
    /// + identity secret) as its identity and `rng` for key generation.
    pub fn new(store: SqliteStore, credentials: Credentials, rng: Rng) -> Result<CryptoGroups, StdError> {
        let spaces_store = NoetaSpacesStore::new(store.clone());
        let forge = NoetaForge::new(store.clone(), credentials.signing_key());
        let manager = NoetaManager::new(spaces_store.clone(), forge, credentials, rng)
            .map_err(|e| io_error(format!("cannot start group encryption: {e}")))?;
        Ok(CryptoGroups {
            manager,
            store,
            spaces_store,
        })
    }

    /// This actor's id (its verifying key) — the same key that identifies the node on the transport
    /// when `credentials` are derived from the node's persisted identity.
    pub fn id(&self) -> ActorId {
        self.manager.id()
    }

    /// Forge this node's key-bundle message. Publishing it lets peers encrypt group secrets toward
    /// us; a receiving peer feeds it to [`Self::receive`] (a `SpacesArgs::KeyBundle` operation).
    pub async fn key_bundle_message(&self) -> Result<SpacesOp, StdError> {
        self.manager
            .key_bundle_message()
            .await
            .map_err(|e| io_error(format!("cannot forge key bundle: {e}")))
    }

    /// Create a space with `initial` members, persisting the resulting auth + space state. Returns
    /// the control operations to replicate to peers.
    pub async fn create_space(
        &self,
        space_id: SpaceId,
        initial: &[(ActorId, Access<Conditions>)],
    ) -> Result<Vec<SpacesOp>, StdError> {
        let (groups_y, space_y, messages) = self
            .manager
            .create_space(space_id, initial)
            .await
            .map_err(|e| io_error(format!("cannot create space: {e}")))?;
        persist_groups(&self.spaces_store, &groups_y)
            .await
            .map_err(|e| io_error(format!("cannot persist auth state: {e}")))?;
        persist_space(&self.spaces_store, space_y)
            .await
            .map_err(|e| io_error(format!("cannot persist space state: {e}")))?;
        Ok(messages)
    }

    /// Encrypt `plaintext` toward the space's members and persist the resulting space state. Returns
    /// the encrypted application operation to replicate.
    pub async fn publish(&self, space_id: SpaceId, plaintext: &[u8]) -> Result<SpacesOp, StdError> {
        let space = self
            .manager
            .space(space_id)
            .await
            .map_err(|e| io_error(format!("cannot open space: {e}")))?
            .ok_or_else(|| io_error("cannot publish to unknown space".to_string()))?;
        let (space_y, message) = space
            .publish(plaintext)
            .await
            .map_err(|e| io_error(format!("cannot encrypt space state: {e}")))?;
        persist_space(&self.spaces_store, space_y)
            .await
            .map_err(|e| io_error(format!("cannot persist space state: {e}")))?;
        Ok(message)
    }

    /// Add `member` to the space at `access`, persisting the resulting auth + space state. Returns
    /// the auth + space-membership operations to replicate (the latter welcomes `member` with the
    /// group key material encrypted toward them).
    pub async fn add(
        &self,
        space_id: SpaceId,
        member: ActorId,
        access: Access<Conditions>,
    ) -> Result<Vec<SpacesOp>, StdError> {
        let space = self
            .manager
            .space(space_id)
            .await
            .map_err(|e| io_error(format!("cannot open space: {e}")))?
            .ok_or_else(|| io_error("cannot add to unknown space".to_string()))?;
        let (groups_y, space_y, auth_message, space_message) = space
            .add(member, access)
            .await
            .map_err(|e| io_error(format!("cannot add member: {e}")))?;
        persist_groups(&self.spaces_store, &groups_y)
            .await
            .map_err(|e| io_error(format!("cannot persist auth state: {e}")))?;
        persist_space(&self.spaces_store, space_y)
            .await
            .map_err(|e| io_error(format!("cannot persist space state: {e}")))?;
        Ok(vec![auth_message, space_message])
    }

    /// Ingest an operation received from a peer: persist it to the log (so dependency lookups
    /// resolve), process it through the manager (decrypt / apply membership), and persist any auth /
    /// space state the manager produced. Returns the decrypted application / membership events.
    ///
    /// Operations must be fed in causal order (each author's log is already ordered by the transport;
    /// application data received before the receiver is welcomed surfaces no events until the
    /// welcoming membership operation arrives).
    pub async fn receive(&self, op: &SpacesOp) -> Result<Vec<Event<Conditions>>, StdError> {
        let permit = self
            .store
            .begin()
            .await
            .map_err(|e| io_error(format!("cannot begin store txn: {e}")))?;
        self.store
            .insert_operation(&op.0.hash, &op.0, &SPACES_LOG_ID)
            .await
            .map_err(|e| io_error(format!("cannot store received operation: {e}")))?;
        self.store
            .commit(permit)
            .await
            .map_err(|e| io_error(format!("cannot commit store txn: {e}")))?;

        let (groups_y, space_y, events) = self
            .manager
            .process(op)
            .await
            .map_err(|e| io_error(format!("cannot process spaces operation: {e}")))?;
        if let Some(groups_y) = groups_y {
            persist_groups(&self.spaces_store, &groups_y)
                .await
                .map_err(|e| io_error(format!("cannot persist auth state: {e}")))?;
        }
        if let Some(space_y) = space_y {
            persist_space(&self.spaces_store, space_y)
                .await
                .map_err(|e| io_error(format!("cannot persist space state: {e}")))?;
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2panda_store::SqliteStoreBuilder;

    /// A fresh in-process peer: a [`CryptoGroups`] over its own in-memory store and a random
    /// identity. Production binds `credentials` to the node's persisted key; the spike only needs a
    /// distinct identity per peer.
    async fn peer() -> CryptoGroups {
        let rng = Rng::default();
        let credentials = Credentials::from_rng(&rng).unwrap();
        let store = SqliteStoreBuilder::memory().build().await.unwrap();
        CryptoGroups::new(store, credentials, rng).unwrap()
    }

    /// Round-trip an operation through the transport wire format — exactly what a peer receives off
    /// the durable transport. Exercising every hand-off through this proves the CBOR encoding is
    /// faithful (hash, signature and extensions survive) end-to-end.
    fn wire(op: &SpacesOp) -> SpacesOp {
        SpacesOp::from_wire(&op.to_wire()).expect("operation round-trips through the wire")
    }

    /// P3.4b.0 spike, now on the **production** path (no `test_utils`) and through the **wire format**
    /// (P3.4b.1.2): a real p2panda-spaces group, in process, encrypts and decrypts application data
    /// across two peers, persisting all state through the public store traits, with every operation
    /// serialized and reconstructed as it would be over the transport. Alice creates a space,
    /// publishes encrypted state, then adds Bob; Bob replays the operations in order, becomes
    /// welcomed, and the buffered application message decrypts to the original plaintext — proving
    /// our Forge / message / store / persistence / wire assembly end-to-end.
    #[test]
    fn two_peers_exchange_encrypted_application_data() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let alice = peer().await;
            let bob = peer().await;
            let bob_id = bob.id();

            // Exchange key bundles as real `KeyBundle` operations (the production path — no
            // in-process `register_member` shortcut): each peer forges its bundle, the other ingests
            // it so it can encrypt group secrets toward the sender.
            let alice_kb = alice.key_bundle_message().await.unwrap();
            let bob_kb = bob.key_bundle_message().await.unwrap();
            bob.receive(&wire(&alice_kb)).await.unwrap();
            alice.receive(&wire(&bob_kb)).await.unwrap();

            // Alice creates the space (she is the sole initial member), then replays the create
            // operations to Bob so he tracks the (public) group membership.
            let space_id = SpaceId::digest(b"room");
            let create_msgs = alice.create_space(space_id, &[]).await.unwrap();
            for msg in &create_msgs {
                bob.receive(&wire(msg)).await.unwrap();
            }

            // Alice publishes encrypted application data, then adds Bob as a reader.
            let plaintext = b"secret convergent state".to_vec();
            let publish_msg = alice.publish(space_id, &plaintext).await.unwrap();
            let add_msgs = alice.add(space_id, bob_id, Access::read()).await.unwrap();

            // Bob receives the ciphertext before being welcomed (buffered — no events yet), then the
            // add operations welcome him and the buffered application data decrypts.
            assert!(
                bob.receive(&wire(&publish_msg)).await.unwrap().is_empty(),
                "not welcomed yet"
            );
            let mut events = Vec::new();
            for msg in &add_msgs {
                events.extend(bob.receive(&wire(msg)).await.unwrap());
            }

            let decrypted = events.iter().find_map(|e| match e {
                Event::Application { data, .. } => Some(data.clone()),
                _ => None,
            });
            assert_eq!(
                decrypted.as_deref(),
                Some(plaintext.as_slice()),
                "Bob decrypts Alice's application data once welcomed into the space"
            );
        });
    }
}
