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

- **P3.4b.0 — offline spike.** Two in-process peers, real `Manager` + `SqliteSpacesStore` + a
  `Forge`: create a space, add peer B, A `publish`es encrypted state, B `process`es + decrypts. No
  networking. Proves the assembly + pins the concrete types. (De-risks everything.)
- **P3.4b.1 — encrypted durable path.** Bind the spike to `RealHost`/the node as a parallel encrypted
  transport; keep the plaintext path unchanged. Encrypt/decrypt `synced_signal` bodies through a space.
- **P3.4b.2 — membership + surface.** `.members()/.encrypted()`; add/remove with key rotation; the
  control-message replication + causal ordering over the topic.
- **P3.4b.3 — multi-node tests.** Two real nodes in a group converge on encrypted state; a
  non-member node on the topic gets ciphertext it cannot read; a removed member stops decrypting new
  state. The real end-to-end proof (`#[ignore]`, run explicitly, like the other real-network tests).
- **P3.4b.4 — packaging.** New deps (`p2panda-spaces`/`-auth`/`-encryption`, store `spaces` feature)
  under the existing default-on `ring-p2p`; confirm the default build stays p2panda-free and the
  footprint scan sheds the whole tree.

## Honest risk

Security-critical + pre-1.0 API with sparse docs. `space.add`/`process_auth_message` already expose
real intricacy (orderer heads/dependencies, encryption-context secret-member changes, per-change
`direct_messages`). This deserves careful, well-tested slices — not a rushed single drop. The
plaintext P3.2 transport stays the always-available default, so this rides entirely behind opt-in
groups and never destabilizes the verified path.
