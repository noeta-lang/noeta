//! Group encryption for `synced_signal` (p2p P3.4b) — the p2panda-spaces assembly.
//!
//! `synced_signal`'s bytes cross the wire in the clear on the P3.2 durable transport. A *group*
//! `synced_signal` instead encrypts every state it publishes to a **space**: a p2panda-spaces
//! [`Space`] is an auth-controlled membership group ([`p2panda_auth`]) with an encryption context
//! ([`p2panda_encryption`]'s symmetric "data encryption" scheme — XChaCha20-Poly1305, so a member
//! that joins late still decrypts prior state, exactly what a convergent CRDT needs). We do not
//! hand-roll crypto; we assemble p2panda's pieces:
//!
//! - a **[`Manager`]** owns the group + encryption state, backed by a [`SqliteSpacesStore`] (the
//!   six storage traits it needs, provided by `p2panda-store`'s `spaces` feature);
//! - a **[`NoetaForge`]** mints the signed operations that carry spaces control/data messages (their
//!   [`SpacesArgs`] ride as the operation header's extensions), persisting them to the log store;
//! - **[`SpacesOp`]** is the message newtype: `Forge::Message` must be `Borrow<SpacesArgs>`, and the
//!   orphan rule blocks impl'ing that on the foreign `Operation` directly, so we wrap it.
//!
//! Local state changes (`create_space`/`space.publish`/`space.add`) return operations to replicate;
//! received operations are fed to [`Manager::process`] **in causal order**, and decrypted
//! application data surfaces as [`Event::Application`]. The state each call returns is persisted
//! through the manager's public `set_groups_state`/`set_space_state` setters (this module mirrors
//! the crate's own test-only `*_persisted` wrappers, which are just that pattern).
//!
//! This slice (P3.4b.0) proves the assembly end-to-end **in process, no networking** — the
//! de-risking spike. Binding it to the real node transport is P3.4b.1.

use std::borrow::Borrow;

use p2panda_core::traits::{Digest, Provenance};
use p2panda_core::{Hash, Header, Operation, SigningKey, VerifyingKey};
use p2panda_spaces::{Forge, SpacesArgs};
use p2panda_store::logs::LogStore;
use p2panda_store::operations::OperationStore;
use p2panda_store::spaces::SqliteSpacesStore;
use p2panda_store::{SqliteError, SqliteStore, tx};

/// We don't use conditional access, so the spaces "conditions" type is unit.
pub type Conditions = ();
/// Our operations carry the spaces control/data args as their header extensions.
pub type SpacesExtensions = SpacesArgs<Conditions>;
type SpacesOperation = Operation<SpacesExtensions>;

/// Every node keeps a single append-only log for its own spaces operations.
const SPACES_LOG_ID: u32 = 0;

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

/// The spaces store type: the SQLite-backed store implementing the six traits [`Manager`] needs.
pub type NoetaSpacesStore = SqliteSpacesStore<SpacesExtensions>;

/// The fully-applied [`Manager`] type for our group encryption.
pub type NoetaManager = p2panda_spaces::manager::Manager<
    NoetaSpacesStore,
    NoetaForge,
    Conditions,
    p2panda_spaces::StrongRemoveResolver<Conditions>,
>;

// The spike drives the raw `Manager`, whose state-persistence helpers are gated behind
// p2panda-spaces' `test_utils` — exposed here only under the dev-only `ring-p2p-testkit` feature.
#[cfg(all(test, feature = "ring-p2p-testkit"))]
mod tests {
    use super::*;
    use p2panda_auth::Access;
    use p2panda_encryption::Rng;
    use p2panda_spaces::Event;
    use p2panda_spaces::{Credentials, SpaceId};
    use p2panda_store::{SqliteStoreBuilder, Transaction};

    /// One in-process peer: a manager over its own store, plus the base store (for inserting
    /// operations received from a peer so dependency lookups resolve).
    struct CryptoPeer {
        manager: NoetaManager,
        store: SqliteStore,
    }

    impl CryptoPeer {
        async fn new() -> CryptoPeer {
            let rng = Rng::default();
            let credentials = Credentials::from_rng(&rng).unwrap();
            let store = SqliteStoreBuilder::memory().build().await.unwrap();
            let spaces_store = NoetaSpacesStore::new(store.clone());
            let forge = NoetaForge::new(store.clone(), credentials.signing_key());
            let manager = NoetaManager::new(spaces_store, forge, credentials, rng).unwrap();
            CryptoPeer { manager, store }
        }

        /// Persist a received operation, then process it, persisting any resulting group/space
        /// state — the production analogue of the crate's test-only `process_persisted`.
        async fn receive(&self, op: &SpacesOp) -> Vec<Event<Conditions>> {
            let permit = self.store.begin().await.unwrap();
            self.store
                .insert_operation(&op.0.hash, &op.0, &SPACES_LOG_ID)
                .await
                .unwrap();
            self.store.commit(permit).await.unwrap();

            let (groups_y, space_y, events) = self.manager.process(op).await.unwrap();
            if let Some(groups_y) = groups_y {
                self.manager.set_groups_state(&groups_y).await.unwrap();
            }
            if let Some(space_y) = space_y {
                let space_id = space_y.space_id;
                self.manager
                    .set_space_state(&space_id, &space_y.into())
                    .await
                    .unwrap();
            }
            events
        }
    }

    /// P3.4b.0 spike: a real p2panda-spaces group, in process, encrypts and decrypts application
    /// data across two peers. Alice creates a space, publishes encrypted state, then adds Bob;
    /// Bob replays the control messages in order, becomes welcomed, and the buffered application
    /// message decrypts to the original plaintext — proving our Forge / message / store assembly.
    #[test]
    fn two_peers_exchange_encrypted_application_data() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let alice = CryptoPeer::new().await;
            let bob = CryptoPeer::new().await;

            // Exchange key bundles (in-process: register each other's published member directly).
            let bob_member = bob.manager.me().await.unwrap();
            let alice_member = alice.manager.me().await.unwrap();
            alice.manager.register_member(&bob_member).await.unwrap();
            bob.manager.register_member(&alice_member).await.unwrap();
            let bob_id = bob.manager.id();

            // Alice creates the space (persist the returned state, like `create_space_persisted`).
            let space_id = SpaceId::digest(b"room");
            let (groups_y, space_y, create_msgs) =
                alice.manager.create_space(space_id, &[]).await.unwrap();
            alice.manager.set_groups_state(&groups_y).await.unwrap();
            alice
                .manager
                .set_space_state(&space_id, &space_y.into())
                .await
                .unwrap();

            // Bob replays the two create messages (auth, then space).
            for msg in &create_msgs {
                bob.receive(msg).await;
            }

            // Alice publishes encrypted application data...
            let plaintext = b"secret convergent state".to_vec();
            let space = alice.manager.space(space_id).await.unwrap().unwrap();
            let (space_y, publish_msg) = space.publish(&plaintext).await.unwrap();
            alice
                .manager
                .set_space_state(&space_id, &space_y.into())
                .await
                .unwrap();

            // ...then adds Bob as a reader (auth message + space membership message with the key
            // material welcoming Bob).
            let (groups_y, space_y, add_auth, add_space) =
                space.add(bob_id, Access::read()).await.unwrap();
            alice.manager.set_groups_state(&groups_y).await.unwrap();
            alice
                .manager
                .set_space_state(&space_id, &space_y.into())
                .await
                .unwrap();

            // Bob receives the application message first (buffered — he is not welcomed yet), then
            // the add messages; once welcomed, the buffered application data decrypts.
            assert!(bob.receive(&publish_msg).await.is_empty(), "not welcomed yet");
            bob.receive(&add_auth).await;
            let events = bob.receive(&add_space).await;

            let decrypted = events.iter().find_map(|e| match e {
                Event::Application { data, .. } => Some(data.clone()),
                _ => None,
            });
            assert_eq!(
                decrypted.as_deref(),
                Some(plaintext.as_slice()),
                "Bob decrypts Alice's application data once welcomed into the space"
            );

            // Bob is now a member of the space.
            let bob_space = bob.manager.space(space_id).await.unwrap().unwrap();
            let members = bob_space.members().await.unwrap();
            assert!(members.iter().any(|(id, _)| *id == bob_id));
        });
    }
}
