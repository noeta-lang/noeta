# P3.4b — group encryption for `synced_signal` (scope + grounded findings)

> Status: **scoped, not started.** P3.4a (per-app data-dir namespacing) is done + committed. This
> doc captures the concrete p2panda-0.7 investigation so the encryption arc can start from facts,
> not the design-doc's one-line `.members().encrypted()`.

## What p2panda actually provides (0.7.0)

Encryption is **not** in the high-level `p2panda` meta-crate yet — its own docs say persistent
storage covers ops/sync/addresses "**and soon** encryption/access-control state." But the building
blocks are all published and usable:

- **`p2panda-encryption`** — the crypto: two schemes. *Data Encryption* (symmetric group key,
  XChaCha20-Poly1305; **late joiners can decrypt history** — the right fit for a CRDT `synced_signal`)
  and *Message Encryption* (Signal-style forward-secure ratchet). Post-compromise security; HPKE +
  libcrux + x25519 under the hood. We do **not** hand-roll any of this.
- **`p2panda-auth`** — group membership / access control (`Access`, add/remove, strong-remove).
- **`p2panda-spaces`** — the high-level composition: a `Space` (= auth group + encryption context).
  `manager.create_space()`, `space.add(member, access)`, `space.remove(member)`, `space.publish(data)`
  (encrypt) → messages to replicate; `Manager::process(msg)` on receive. `StrongRemoveResolver` and
  `Config` are provided.
- **`p2panda-store` `spaces` feature** — `SqliteSpacesStore` implements all six store traits the
  `Manager` needs (`SpacesStore`, `SpacesMessageStore`, `GroupsStore`, `KeyRegistryStore`,
  `KeySecretsStore`, `Transaction`). **The store is provided**, not hand-written.
- **`p2panda-stream` `spaces`/`groups` features** — the *production* pipeline: `orderer::Orderer`
  (causal ordering — **required**: `Manager` docs say "all messages must be ordered according to
  their causal relationship before being processed"), a `Groups` processor, a `spaces` processor,
  `ingest`, `log_prune`. This is what turns raw received operations into ordered `process()` calls.

## So B = *assembly*, but it is the arc's single largest slice

We compose p2panda's pieces rather than invent crypto. What we must build/wire:

1. **Message type (orphan-rule).** `Forge::Message` must impl `Borrow<SpacesArgs<C>> + Provenance +
   Digest`. `Operation<SpacesArgs<C>>` is foreign, so we can't impl `Borrow` for it directly — use
   `p2panda-stream`'s provided `GroupsOperation<C>` message, or a local newtype over `Operation`.
2. **A `Forge`** (~40 lines, template in spaces `test_utils`): builds+signs+persists an operation
   whose header `extensions` carry the `SpacesArgs`. Spaces control-messages *are* operations in the
   same log system as data — aligns with our existing `LogSync` operation model.
3. **The encrypted transport path.** A `synced_signal` created with a group runs a **parallel
   pipeline** to the plaintext P3.2 path (which stays untouched + verified): `space.publish(state)`
   to encrypt before the wire; on receive, feed operations through `Orderer` → `Manager::process` →
   decrypt → merge. Control ops (auth + space membership, incl. key `direct_messages` on membership
   change) replicate over the same topic and must be processed in causal order.
4. **Language surface.** `synced_signal(initial, topic).members([...]).encrypted()` (or a config
   object), safe-by-default (encrypted + explicit membership). Non-member cannot decrypt.
5. **Identity.** The group actor id is the node's persisted Ed25519 key (P3.3) — already in place.

## Staging (each a green, committed increment)

- **P3.4b.0 — offline spike. ✅ DONE** (`a01e5a58`). Two in-process peers, real `Manager` +
  `SqliteSpacesStore` + our `NoetaForge`/`SpacesOp`: A creates a space, publishes encrypted state,
  adds B; B replays control messages in causal order and, once welcomed, decrypts. Proved the
  assembly + pinned the types. **Finding:** the raw `Manager`'s state-persistence helpers
  (`set_groups_state`/`set_space_state`/`*_persisted`) are `#[cfg(test/test_utils)]` — so the
  **production integration persists via `p2panda-stream`'s spaces processor + orderer**, not the raw
  Manager. b.1 adopts that. (The spike used a dev-only `ring-p2p-testkit` feature for those helpers.)
- **P3.4b.1 — encrypted durable path. ✅ DONE.** Bound the assembly to the node behind the public
  store traits, no `test_utils`. Three green sub-slices:
  - **b.1.0** (`869bbbc8`): `CryptoGroups` — the production component. The manager is store-backed and
    stateless between calls, so persist each returned auth/space delta via the **public**
    `set_*_state_tx` store traits (`persist_groups`/`persist_space`), mirroring the crate's test-only
    `*_persisted`. Key bundles flow as real `KeyBundle` operations. Dropped the `ring-p2p-testkit`
    crutch — the spike test now runs on the production path.
  - **b.1.1** (`e63b5c28`): node owns a lazily-built `CryptoGroups` whose actor id **is** the node
    identity (`Credentials::from_keys(node_key, x25519_secret)`), with the whole `Credentials`
    persisted via serde (`credentials.key`, 0600) since the x25519 secret can't be read out as bytes;
    isolated `spaces.db` store. `crypto_group_id() == identity()`, persists across restart.
  - **b.1.2** (`f9eb76b8`): `SpacesOp::to_wire`/`from_wire` — spaces ops carry all data (incl.
    ciphertext) in the header extensions with an empty body, so the CBOR header **is** the wire form;
    hash recomputed + signature verified on receipt. The e2e crypto test routes every hand-off
    through the wire.
- **P3.4b.2 — membership + surface. ✅ DONE.** Encryption is *transparent to program semantics* (the
  decrypted value equals the plaintext value), so the deterministic **sandbox treats an encrypted
  synced_signal as a pass-through**, keeping the oracle byte-identical; only `RealHost` does real
  crypto (mirrors `p2p_identity`/`sync_status`). Sub-slices:
  - **b.2.0** (`3e1ba50a`): surface — `synced_signal(initial, topic, members)`. Config at
    construction (a builder chain would leak the initial announce in the clear before encryption was
    set up); members-imply-encryption is the safe default. `encrypted.noe` fixture converges like the
    plaintext twin.
  - **b.2.1** (`3577536e`): host seam — `p2p_group_open`/`_publish`/`_poll` with transparent
    pass-through defaults; `synced_signal` routes through them when encrypted.
  - **b.2.2** (`2378574f`): RealHost crypto — `EncryptedGroup`, a **transport-independent** model-A
    state machine (creator = min member id, on-topic key-bundle announce, welcome as bundles arrive,
    non-creator buffers state until welcomed then flushes). Node `group_*` wire it to the transport
    (async crypto in one `block_on`, transport publishes after — never nested). Hermetic in-memory
    relay test proves the whole handshake + convergence.
- **P3.4b.3 — multi-node test. ✅ DONE** (`0a6c0395`): two **real** p2panda nodes open the same
  encrypted group and converge on encrypted state over live QUIC/iroh — creator election, key-bundle
  exchange, welcome, decryption end-to-end. `#[ignore]` (real mDNS), but **passes** when run
  explicitly. (Non-member-cannot-read is inherent — a non-member has no group key; removed-member is
  part of the deferred dynamic-membership follow-up, see below.)
- **P3.4b.4 — packaging. ✅ DONE.** `p2panda-spaces`/`-auth`/`-encryption` (+ store `spaces` feature,
  + `postcard`) all sit under the default-on `ring-p2p`; verified `noeta-runtime` **and**
  `noeta-aot-runtime` `--no-default-features` trees contain **0** p2panda/iroh/spaces crates, so the
  footprint scan sheds the whole tree from a tailored native binary.

- **P3.4b.2.3 — dynamic membership + revocation. ✅ DONE** (`272e5116` crypto, `22665539` surface).
  Runtime `.add_member(peer_id)` / `.remove_member(peer_id)` on an encrypted synced_signal. `remove`
  **rotates the group key** — p2panda-spaces performs the rotation (`apply_secret_member_change`), so
  a removed peer cannot decrypt state published afterward. `EncryptedGroup` tracks `known_bundles`
  (welcome a runtime-added member immediately if its bundle already arrived) and skips undecryptable
  application ciphertext silently (a non-member/revoked peer never sees plaintext, no error). The
  creator is authoritative over membership; sandbox = no-op (transparent to the converged value).
  Proven by the hermetic `removed_member_cannot_decrypt_new_state` test **and** the real-network
  `two_nodes_revocation_over_the_wire` test (passes over QUIC).

## Status: arc complete

P3.4b (group encryption) is complete end-to-end — assembly, node identity, wire format, surface,
choreography, dynamic membership + revocation — with hermetic and real-network proofs. Nothing
deferred.

## Honest risk

Security-critical + pre-1.0 API with sparse docs. `space.add`/`process_auth_message` already expose
real intricacy (orderer heads/dependencies, encryption-context secret-member changes, per-change
`direct_messages`). This deserves careful, well-tested slices — not a rushed single drop. The
plaintext P3.2 transport stays the always-available default, so this rides entirely behind opt-in
groups and never destabilizes the verified path.
