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
use noeta_ext_abi::StdError;

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

    /// Whether this is an encrypted **application** operation (as opposed to a control operation —
    /// key bundle, auth, or space-membership). Only application ciphertext can be undecryptable for
    /// us (a non-member or revoked peer), so this gates the "skip silently" path on receive.
    pub fn is_application(&self) -> bool {
        matches!(self.0.header.extensions, SpacesArgs::Application { .. })
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
                &self.store,
                &self.signing_key.verifying_key(),
                &SPACES_LOG_ID,
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
    pub fn new(
        store: SqliteStore,
        credentials: Credentials,
        rng: Rng,
    ) -> Result<CryptoGroups, StdError> {
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

    /// Remove `member` from the space, persisting the resulting auth + space state. p2panda-spaces
    /// **rotates the group encryption secret** as part of this, so the removed member cannot decrypt
    /// any state published after removal (revocation). Returns the auth + space-membership operations
    /// to replicate (the latter carries the rotated key material to the remaining members).
    pub async fn remove(
        &self,
        space_id: SpaceId,
        member: ActorId,
    ) -> Result<Vec<SpacesOp>, StdError> {
        let space = self
            .manager
            .space(space_id)
            .await
            .map_err(|e| io_error(format!("cannot open space: {e}")))?
            .ok_or_else(|| io_error("cannot remove from unknown space".to_string()))?;
        let (groups_y, space_y, auth_message, space_message) = space
            .remove(member)
            .await
            .map_err(|e| io_error(format!("cannot remove member: {e}")))?;
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

    /// Whether `actor` is currently a member of the space (used to detect being welcomed). A space
    /// this node hasn't created/joined yet answers `false`.
    pub async fn is_member(&self, space_id: SpaceId, actor: ActorId) -> Result<bool, StdError> {
        let Some(space) = self
            .manager
            .space(space_id)
            .await
            .map_err(|e| io_error(format!("cannot open space: {e}")))?
        else {
            return Ok(false);
        };
        let members = space
            .members()
            .await
            .map_err(|e| io_error(format!("cannot read space members: {e}")))?;
        Ok(members.iter().any(|(id, _)| *id == actor))
    }
}

/// The wire operations to broadcast and the decrypted application payloads produced by one step of
/// the [`EncryptedGroup`] choreography.
#[derive(Debug, Default)]
pub struct GroupOutput {
    /// Decrypted application state to surface to the program (fed to the CRDT merge).
    pub decrypted: Vec<Vec<u8>>,
    /// Wire operations to broadcast to the topic (a creator's welcome ops, a flushed publish).
    pub to_send: Vec<Vec<u8>>,
}

/// The membership choreography for one encrypted `synced_signal` group (p2p P3.4b, model A),
/// **independent of the transport** — feed it operations received off the wire, it tells you what
/// to send back and yields decrypted payloads. Keeping it transport-free is what makes the
/// security-critical handshake unit-testable without real networking (see the in-memory relay test).
///
/// Model A: the group's member set is fixed at construction. The **creator** — the
/// lexicographically smallest member id, which every node computes identically from the shared set —
/// creates the space and welcomes each other member as that member's key bundle arrives on the
/// topic. A non-creator announces its key bundle and waits to be welcomed; any state it wants to
/// publish before then is buffered (latest-wins, a CRDT) and flushed once welcomed.
#[derive(Debug)]
pub struct EncryptedGroup {
    crypto: CryptoGroups,
    space_id: SpaceId,
    members: Vec<ActorId>,
    me: ActorId,
    creator: bool,
    welcomed: bool,
    /// Members already welcomed by this node (creator only), so each is added exactly once.
    added: std::collections::HashSet<ActorId>,
    /// Every peer whose key bundle we have processed (member or not). Lets a member added at runtime
    /// (`add_member`) be welcomed immediately if its bundle already arrived, not only when a fresh
    /// bundle event fires.
    known_bundles: std::collections::HashSet<ActorId>,
    /// The latest local state awaiting a welcome (non-creator), flushed when welcomed. Latest-wins
    /// is safe: the value is a CRDT, so the most recent local state subsumes earlier ones.
    pending: Option<Vec<u8>>,
}

impl EncryptedGroup {
    /// Open the encrypted group for `topic` with the given `members`. Returns the group and the
    /// initial operations to broadcast: this node's key bundle, plus (if it is the elected creator)
    /// the space-creation operations.
    pub async fn open(
        crypto: CryptoGroups,
        topic: &str,
        members: Vec<ActorId>,
    ) -> Result<(EncryptedGroup, Vec<Vec<u8>>), StdError> {
        let me = crypto.id();
        let space_id = SpaceId::digest(topic.as_bytes());
        // Deterministic creator election: the smallest member id. Every node computes the same
        // answer from the shared member set, with no coordination.
        let creator = members.iter().min().copied() == Some(me);

        let mut to_send = Vec::new();
        // Announce our key bundle so members can encrypt group secrets toward us.
        let key_bundle = crypto.key_bundle_message().await?;
        to_send.push(key_bundle.to_wire());

        let mut group = EncryptedGroup {
            crypto,
            space_id,
            members,
            me,
            creator,
            welcomed: false,
            added: std::collections::HashSet::new(),
            known_bundles: std::collections::HashSet::new(),
            pending: None,
        };

        if creator {
            // Create the space; the creator is auto-added as manager, so it is welcomed immediately.
            let create_ops = group.crypto.create_space(space_id, &[]).await?;
            for op in &create_ops {
                to_send.push(op.to_wire());
            }
            group.welcomed = true;
            group.added.insert(me);
        }

        Ok((group, to_send))
    }

    /// Publish local `plaintext` state to the group. Once welcomed it is encrypted and returned as a
    /// wire operation to broadcast; before then it is buffered (latest-wins) and flushed on welcome.
    pub async fn publish(&mut self, plaintext: Vec<u8>) -> Result<Vec<Vec<u8>>, StdError> {
        if self.welcomed {
            let op = self.crypto.publish(self.space_id, &plaintext).await?;
            Ok(vec![op.to_wire()])
        } else {
            self.pending = Some(plaintext);
            Ok(Vec::new())
        }
    }

    /// Process one operation received off the wire. Decrypts application data, drives the creator's
    /// welcome of members whose key bundles arrive, and (for a non-creator) flushes buffered state
    /// once this node is welcomed into the space.
    pub async fn receive(&mut self, wire: &[u8]) -> Result<GroupOutput, StdError> {
        let op = SpacesOp::from_wire(wire)?;
        let events = match self.crypto.receive(&op).await {
            Ok(events) => events,
            // Encrypted application data we cannot decrypt — we are not (or no longer) a member, or
            // lack the rotated group key. A non-member/revoked peer simply never sees the plaintext;
            // skip it silently rather than failing the sync. Control ops must always process, so
            // only application ciphertext takes this path.
            Err(_) if op.is_application() => return Ok(GroupOutput::default()),
            Err(e) => return Err(e),
        };

        let mut out = GroupOutput::default();
        for event in events {
            match event {
                Event::Application { data, .. } => out.decrypted.push(data),
                Event::KeyBundle { author } => {
                    self.known_bundles.insert(author);
                }
                _ => {}
            }
        }

        // Creator welcomes any declared member whose key bundle is now known (each exactly once).
        out.to_send.extend(self.welcome_eligible().await?);

        // Non-creator: detect being welcomed, then flush any state we buffered before joining.
        if !self.welcomed && self.crypto.is_member(self.space_id, self.me).await? {
            self.welcomed = true;
            if let Some(plaintext) = self.pending.take() {
                let op = self.crypto.publish(self.space_id, &plaintext).await?;
                out.to_send.push(op.to_wire());
            }
        }

        Ok(out)
    }

    /// Add `member` to the group at runtime (p2p P3.4b dynamic membership). Only the creator manages
    /// membership; on any other node this records the intent but produces nothing (the creator is
    /// authoritative). If the new member's key bundle is already known it is welcomed immediately;
    /// otherwise the welcome fires when its bundle arrives. Returns wire ops to broadcast.
    pub async fn add_member(&mut self, member: ActorId) -> Result<Vec<Vec<u8>>, StdError> {
        if !self.members.contains(&member) {
            self.members.push(member);
        }
        self.welcome_eligible().await
    }

    /// Remove `member` from the group at runtime, **rotating the group key** so the removed member
    /// cannot decrypt state published afterward (revocation — p2panda-spaces performs the rotation).
    /// Only the creator manages membership; a no-op on any other node. Returns wire ops to broadcast.
    pub async fn remove_member(&mut self, member: ActorId) -> Result<Vec<Vec<u8>>, StdError> {
        self.members.retain(|m| *m != member);
        self.added.remove(&member);
        self.known_bundles.remove(&member);
        if !self.creator || member == self.me {
            return Ok(Vec::new());
        }
        let ops = self.crypto.remove(self.space_id, member).await?;
        Ok(ops.iter().map(SpacesOp::to_wire).collect())
    }

    /// Welcome every declared member whose key bundle we hold but who is not yet added (creator
    /// only). Idempotent — each member is added exactly once. Returns wire ops to broadcast.
    async fn welcome_eligible(&mut self) -> Result<Vec<Vec<u8>>, StdError> {
        if !self.creator {
            return Ok(Vec::new());
        }
        let eligible: Vec<ActorId> = self
            .members
            .iter()
            .copied()
            .filter(|m| *m != self.me && self.known_bundles.contains(m) && !self.added.contains(m))
            .collect();
        let mut wire = Vec::new();
        for member in eligible {
            let add_ops = self
                .crypto
                .add(self.space_id, member, Access::read())
                .await?;
            self.added.insert(member);
            for op in &add_ops {
                wire.push(op.to_wire());
            }
        }
        Ok(wire)
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

    /// Pump the two groups to a fixpoint over an in-memory relay: each drains its inbox, its replies
    /// land in the peer's inbox, until nothing flows.
    async fn pump(
        alice: &mut EncryptedGroup,
        bob: &mut EncryptedGroup,
        alice_in: &mut std::collections::VecDeque<Vec<u8>>,
        bob_in: &mut std::collections::VecDeque<Vec<u8>>,
        alice_dec: &mut Vec<Vec<u8>>,
        bob_dec: &mut Vec<Vec<u8>>,
    ) {
        loop {
            let mut progressed = false;
            while let Some(msg) = alice_in.pop_front() {
                let out = alice.receive(&msg).await.unwrap();
                alice_dec.extend(out.decrypted);
                bob_in.extend(out.to_send);
                progressed = true;
            }
            while let Some(msg) = bob_in.pop_front() {
                let out = bob.receive(&msg).await.unwrap();
                bob_dec.extend(out.decrypted);
                alice_in.extend(out.to_send);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    /// P3.4b.2.2: the model-A membership choreography, driven purely through the [`EncryptedGroup`]
    /// state machine over an in-memory relay (no networking) — the hermetic proof of the handshake
    /// that the `#[ignore]` real-node test (b.3) exercises over QUIC. Two members open the group;
    /// the elected creator creates the space and welcomes the other as its key bundle arrives; then
    /// the creator publishes encrypted state and the other member decrypts it. Everything the creator
    /// sends is ciphertext + control ops — the plaintext only ever exists inside a welcomed member.
    #[test]
    fn two_group_members_converge_on_encrypted_state() {
        use std::collections::VecDeque;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let alice_c = peer().await;
            let bob_c = peer().await;
            let members = vec![alice_c.id(), bob_c.id()];

            let (mut alice, a_init) = EncryptedGroup::open(alice_c, "team/secure", members.clone())
                .await
                .unwrap();
            let (mut bob, b_init) = EncryptedGroup::open(bob_c, "team/secure", members)
                .await
                .unwrap();

            // Each node's initial ops go to the other's inbox.
            let mut alice_in: VecDeque<Vec<u8>> = b_init.into();
            let mut bob_in: VecDeque<Vec<u8>> = a_init.into();
            let mut alice_dec: Vec<Vec<u8>> = Vec::new();
            let mut bob_dec: Vec<Vec<u8>> = Vec::new();

            // Settle the handshake: key bundles exchanged, the creator welcomes the other member.
            pump(
                &mut alice,
                &mut bob,
                &mut alice_in,
                &mut bob_in,
                &mut alice_dec,
                &mut bob_dec,
            )
            .await;

            // The creator publishes a secret; the other member decrypts it after the next pump.
            let secret = b"converged secret".to_vec();
            if alice.creator {
                let ops = alice.publish(secret.clone()).await.unwrap();
                bob_in.extend(ops);
            } else {
                let ops = bob.publish(secret.clone()).await.unwrap();
                alice_in.extend(ops);
            }
            pump(
                &mut alice,
                &mut bob,
                &mut alice_in,
                &mut bob_in,
                &mut alice_dec,
                &mut bob_dec,
            )
            .await;

            let received = if alice.creator { &bob_dec } else { &alice_dec };
            assert!(
                received.iter().any(|d| d == &secret),
                "the non-creator member decrypts the creator's encrypted state"
            );
        });
    }

    /// P3.4b dynamic membership: a **removed** member cannot decrypt state published after its
    /// removal — revocation, backed by p2panda-spaces' key rotation. The creator publishes a secret
    /// both members see, removes the other member (rotating the group key), then publishes a second
    /// secret; the removed member receives that ciphertext but cannot decrypt it.
    #[test]
    fn removed_member_cannot_decrypt_new_state() {
        use std::collections::VecDeque;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let alice_c = peer().await;
            let bob_c = peer().await;
            let alice_id = alice_c.id();
            let bob_id = bob_c.id();
            let members = vec![alice_id, bob_id];

            let (mut alice, a_init) = EncryptedGroup::open(alice_c, "team/secure", members.clone())
                .await
                .unwrap();
            let (mut bob, b_init) = EncryptedGroup::open(bob_c, "team/secure", members)
                .await
                .unwrap();
            let mut alice_in: VecDeque<Vec<u8>> = b_init.into();
            let mut bob_in: VecDeque<Vec<u8>> = a_init.into();
            let mut alice_dec: Vec<Vec<u8>> = Vec::new();
            let mut bob_dec: Vec<Vec<u8>> = Vec::new();
            pump(
                &mut alice,
                &mut bob,
                &mut alice_in,
                &mut bob_in,
                &mut alice_dec,
                &mut bob_dec,
            )
            .await;

            // Identify the creator (which manages membership) and the member it will remove.
            let alice_is_creator = alice.creator;
            let removed_id = if alice_is_creator { bob_id } else { alice_id };

            // The creator publishes a first secret both members should decrypt.
            let before = b"visible to both".to_vec();
            let ops = if alice_is_creator {
                alice.publish(before.clone()).await.unwrap()
            } else {
                bob.publish(before.clone()).await.unwrap()
            };
            if alice_is_creator {
                bob_in.extend(ops)
            } else {
                alice_in.extend(ops)
            }
            pump(
                &mut alice,
                &mut bob,
                &mut alice_in,
                &mut bob_in,
                &mut alice_dec,
                &mut bob_dec,
            )
            .await;

            // The creator removes the other member (rotating the key), then publishes a second
            // secret the removed member must not be able to read.
            let after = b"secret after removal".to_vec();
            let mut ops = if alice_is_creator {
                alice.remove_member(removed_id).await.unwrap()
            } else {
                bob.remove_member(removed_id).await.unwrap()
            };
            ops.extend(if alice_is_creator {
                alice.publish(after.clone()).await.unwrap()
            } else {
                bob.publish(after.clone()).await.unwrap()
            });
            if alice_is_creator {
                bob_in.extend(ops)
            } else {
                alice_in.extend(ops)
            }
            pump(
                &mut alice,
                &mut bob,
                &mut alice_in,
                &mut bob_in,
                &mut alice_dec,
                &mut bob_dec,
            )
            .await;

            let removed_dec = if alice_is_creator {
                &bob_dec
            } else {
                &alice_dec
            };
            assert!(
                removed_dec.iter().any(|d| d == &before),
                "the member decrypted state from while it was a member"
            );
            assert!(
                !removed_dec.iter().any(|d| d == &after),
                "the removed member cannot decrypt state published after removal (revocation)"
            );
        });
    }
}
