# P3 — real p2panda transport (scope)

> Status: **scoping only** (no code). The packaging blocker is lifted (the package-manager arc's
> Phase 1 gives dependency-gated in-tree extensions — see [README](README.md) §P3 and
> `plans/package-manager`), so P3 is now a *transport-engineering* slice, not an ecosystem one. The
> one remaining external factor is p2panda's own pre-1.0 maturity. This doc grounds the work in
> p2panda's **actual current state** and stages it behind a Cargo feature so pre-1.0 churn never
> destabilizes the mainline.

## 1. Goal — swap the transport, change nothing else

P0–P2 built the whole model on one seam: the [`P2p`] host capability carrying **opaque bytes**
between replicas, with the sandbox backed by a deterministic in-process broker and `RealHost`
currently backed by *the same* broker (single-node loopback). P3 replaces **only** `RealHost`'s
broker with a real p2panda node. Everything above the seam is untouched:

- `std.p2p` (`publish`/`receive`) and `std.synced` (`synced_signal`/`merge`/`sync`) are already
  bytes-over-`P2p`; they gain real cross-node reach with **no surface change**.
- CRDT wire serialization (P2.0) already turns a `synced_signal`'s value into the bytes that cross
  the wire. P3 just makes those bytes reach another machine.
- The **sandbox stays the deterministic broker** — conformance and the differential are unaffected;
  real transport is `RealHost`-only and never oracle-tested, exactly like `reqwest` for `Network`.

The swap point is precise: `impl P2p for RealHost` in `crates/noeta-runtime/src/lib.rs` (today four
methods delegating to `noeta_stdlib::P2pBroker`).

## 2. p2panda as it actually is (July 2026, grounded — not the design-doc description)

- **`p2panda-net` v0.7.0** (released 2026-07-07). Modular building blocks over **iroh** (QUIC,
  NAT traversal): `Endpoint` (encrypted connections), `Gossip` (ephemeral pub/sub), `LogSync`
  (eventual-consistent append-log sync), `Discovery` / `MdnsDiscovery` (LAN + confidential topic
  discovery), `AddressBook`, `Supervisor` (Erlang-style restart trees).
- **Raw bytes, async streams.** A topic is `Hash::digest(b"name").into()`; the path is
  `AddressBook → Endpoint → Gossip`, then `gossip.stream(topic)` gives `.publish(bytes)` and a
  `.subscribe()` stream of `bytes`. Byte-oriented and CRDT-agnostic — exactly our seam.
- **Pre-1.0, explicitly unstable.** "Core data types and user-facing APIs may undergo breaking
  changes until v1.0.0." And a real friction: p2panda-net pins an **unpublished `iroh-gossip`**, so
  a consumer must carry a `[patch]` in the workspace `Cargo.toml` until it's upstreamed.
- **Two consistency models, and they map to our two surfaces:**
  - **`Gossip`** = ephemeral, best-effort (a late/dropped peer misses messages) → backs `std.p2p`'s
    ephemeral `publish`/`receive` (presence, live signals).
  - **`LogSync`** = append-only-log eventual consistency (a late peer gets all prior state) → backs
    **`synced_signal`**, whose whole point is convergence. This matches P1/P2's broker model
    (append-log + per-subscriber cursor from the start), so the seam already has the right shape.

Sources: [crates.io/p2panda-net](https://crates.io/crates/p2panda-net),
[docs.rs/p2panda-net](https://docs.rs/p2panda-net/latest/p2panda_net/),
[p2panda.org](https://p2panda.org/).

## 3. Mapping the `P2p` seam onto p2panda

| `P2p` trait method (today) | p2panda backing (P3) |
| --- | --- |
| `p2p_publish(topic, bytes)` | `gossip.stream(topic).publish(bytes)` (ephemeral) / append to a `LogSync` log (synced) |
| `p2p_subscribe(topic) -> u64` | `gossip.stream(topic).subscribe()` → store the stream, hand back an id |
| `p2p_poll_sub(sub) -> Option<bytes>` | non-blocking drain of that subscription's channel |
| `p2p_receive(topic) -> ExternIo` | **override** the default with a genuine subscription future (mirrors how `RealHost` overrides `net_accept` with a real `TcpListener` future) |

The seam already anticipates this: `p2p_receive`/`p2p_poll_sub` were designed as override points, and
`RealHost` overriding a default leaf with a real async future is the established pattern
(`net_accept`/`net_fetch`). **No seam change is expected** — the P1/P2 API was shaped for exactly
this substitution. (If P3 reveals a genuine gap, note it: the ABI is deliberately un-frozen, so
extending it is cheap — package-manager arc's explicit choice.)

## 4. The real architectural challenge — a long-lived node vs `RealHost`'s leaf-blocking IO

`RealHost` today drives IO with `block_on` **at the leaf** (per-call), on a per-isolate
`current_thread` runtime. A p2panda node is the opposite shape: **long-lived**, started with the
process, continuously receiving gossip/sync in the background (the design's "the node lives in the
persistent runtime, on its own isolate" — §9.15.1). So P3's core engineering is the **async bridge**:

- Spawn the node + its background tasks once (lazily on first p2p use, or at `serve`/program start),
  living for the process — like the connection pool `RealHost` already holds for `Network`.
- Each subscription's inbound stream drains into a channel; `p2p_poll_sub` is a non-blocking
  `try_recv`, `p2p_receive`'s future is a real `recv().await`. Publishes hand bytes to the node's
  task. This keeps the synchronous `P2p` trait surface intact over an async node.
- Decide the runtime: reuse `RealHost`'s runtime, or give the node a dedicated multi-thread runtime
  (iroh wants its own driver). This is the main open implementation question.

## 5. Packaging (Phase 1 seam — already available)

- `P2pExtension` (the `std.p2p` unit that already holds `crdt`/`p2p`/`synced`) gains **p2panda-net
  as a Cargo-feature-gated native dependency** — e.g. a `p2p-net` feature on `noeta-runtime`. A build
  without it never links iroh/QUIC/crypto (whole-feature exclusion; the DCE ring work, now merged via
  `aot-dce` L3.4, prunes the rest). **No ABI freeze needed** — static cargo composition, per the
  package-manager arc.
- The `iroh-gossip` `[patch]` lives in the workspace `Cargo.toml`, active only under the feature.
- Default builds stay p2panda-free, so a pre-1.0 dependency cannot destabilize the mainline or the
  oracle suite.

## 6. Testing without the oracle

Real transport is non-deterministic → **no conformance/differential coverage** (like `reqwest`).
Coverage is instead:
- **Two-process localhost integration test** (behind the feature): spawn two `noeta run` processes,
  each a replica of a `synced_signal` on a topic, connected via `MdnsDiscovery`; assert they converge
  to the same CRDT value. This is the real end-to-end proof.
- **Sandbox unchanged** — the deterministic broker keeps proving the *language-level* logic; P3 only
  needs to prove *the transport wiring*, a much smaller surface.
- Log/expose enough that a manual two-machine smoke test is easy.

## 7. New surface P3 enables (deferred from P2, now meaningful)

- **`SyncStatus`** (`Synced` / `Syncing` / `Offline`) — meaningless on the loopback broker (always
  Synced), real once there's a network to be offline from. `synced_signal` carries it; an `effect`
  renders "working offline, 3 peers unreachable."
- **Identity** — `identity()` = the node's persisted **Ed25519** keypair (p2panda-core). Needs a
  storage path (config), the first genuinely-persistent runtime state.
- **Topic membership / encryption** — `.members([...]).encrypted()` (§9.15.1): p2panda group
  encryption. Default-safe (encrypted, explicit membership); open public topic for prototyping.
- **Node config** — storage path, discovery methods (mDNS / rendezvous), relay — configured, not
  imperatively constructed (§9.15.1).

## Status (branch `p2p-p3`)

- **P3.0 ✅** node bootstrap behind `ring-p2p`; async bridge (dedicated runtime + drain channels).
- **P3.1 ✅** RealHost `P2p` routes through the node; `p2p_subscribe` made fallible; **verified**
  two-node gossip delivery (mDNS, 3.16s).
- **Packaging ✅** `ring-p2p` default-on, forwarded to `noeta-aot-runtime`, `ring: Some("ring-p2p")`
  on the transport modules; `--no-default-features` sheds all p2panda (per the extension-system
  handoff).
- **P3.2 ✅** durable `synced_signal` transport via p2panda **log-sync** (SqliteStore append-logs of
  signed operations); durable seam (`p2p_*_durable`, sandbox = broker); **verified** a late-joining
  node catches up on a peer's prior state (4.20s) — the eventual-consistency guarantee.
- **Remaining:** P3.3 (persist the Ed25519 identity + `SyncStatus` surface), P3.4 (group encryption
  + topic membership + node config — the large one), P3.5 (docs: flip Local-First "what's next").

## 8. Proposed staging (feature-gated throughout)

- **P3.0 — node bootstrap.** Add `p2panda-net` under a `p2p-net` cargo feature + the `iroh-gossip`
  patch; build/tear-down a node on `RealHost` (identity, endpoint, gossip), no surface change yet.
  Prove it compiles + a node starts. De-risks the dependency (pre-1.0, patch, binary size) first.
- **P3.1 — Gossip → `std.p2p`.** Wire `publish`/`receive` to a gossip topic; the two-process
  localhost integration test exchanging raw messages. First real cross-process delivery.
- **P3.2 — LogSync → `synced_signal`.** Back synced state with `LogSync` so CRDT convergence holds
  across real peers (late-joiner catch-up). Two-process convergence test.
- **P3.3 — identity + `SyncStatus`.** Persisted Ed25519 identity + storage path; sync-status surface.
- **P3.4 — membership + encryption + config.** Safe-by-default encrypted topics; node config surface.
- **P3.5 — packaging + docs polish.** Feature gating verified (default build p2panda-free + size
  check), patch documented, `docs/Local-First-and-P2P.md` "what's next" → "shipped".

## 9. Risks & the go/no-go

- **p2panda pre-1.0 churn.** v0.7.0 *just* released (2026-07-07); breaking changes expected pre-1.0.
  Mitigation: pin the exact version, keep everything behind the `p2p-net` feature and out of default
  builds, so churn is quarantined. This is the dominant risk and the reason to stage a spike first.
- **The `iroh-gossip` patch.** A workspace `[patch]` is required until upstreamed — fragile across
  updates. Acceptable behind a feature; document it.
- **Binary size / build cost.** iroh + QUIC + crypto is multiple MB and a heavy compile. Feature
  gating + DCE keep it off non-p2p builds; confirm with a size check in P3.5.
- **No oracle coverage** for the real path — mitigated by the two-process integration tests + the
  unchanged deterministic sandbox proving the logic.
- **Async bridge complexity** (§4) — the one genuinely new architecture; the P3.0 spike proves it.

**Recommendation.** Do a **minimal spike — P3.0 + P3.1** — behind the `p2p-net` feature: bootstrap a
real node and prove *one* real cross-process gossip exchange with a two-process test. That validates
the whole bet (the seam holds against real p2panda; the async bridge works; the packaging gates
cleanly) at low blast radius. **Then decide** whether to push through LogSync/synced-signal
integration (P3.2+) now or park until p2panda hits 1.0 — a decision best made *after* the spike shows
how stable v0.7.0 is in practice, rather than committing the full arc up front against a moving
pre-1.0 dependency.
