# Native p2p / local-first module — scope

> Status: **scoping only** (no code). Traces the original design (`docs/resources/01-architecture.md`
> §9.15 / §9.15.1 / §9.15.2, dropped in `085d0b0`, recovered from history) onto the seams that
> actually exist today, and stages the work. This is a flagged **R&D direction**, opt-in per app —
> not a committed near-term milestone.

## 1. What was proposed

Three layers, explicitly divided (§9.15):

- **Networking / identity / sync / encryption** — the genuinely hard part (NAT traversal, discovery,
  transport, identity, group encryption). Bring in [**p2panda**](https://p2panda.org) (modular Rust
  crates over iroh/QUIC: mDNS + rendezvous discovery, NAT-traversing transport + relays, gossip
  pub/sub, append-log sync, blob transfer, Ed25519 identity, group encryption). Data-type- and
  CRDT-agnostic (operates over raw bytes), transport-independent.
- **Convergence (CRDTs)** — a `Mergeable` trait backed by a CRDT (Automerge/Loro/p2panda types).
  Opt-in per value, never universal.
- **The language = the integration** — signals react to synced state; the persistent runtime hosts
  the embedded node on its own isolate; Tauri packages it.

Surface sketch (§9.15.1) — *"signals that happen to be shared, not a networking library bolted on"*:

- `synced_signal(initial, topic: "room:42/counter")` — same `.get()/.set()/.update()` as a local
  `signal`, but changes propagate to peers on the topic and theirs flow back.
- **Convergence is a compile-time trait.** A synced value's type must be `Mergeable` — you cannot
  sync a type with no convergence story (illegal-states-unrepresentable applied to sync).
- **`SyncedSignal<T>` is a distinct type from `Signal<T>`** — the network boundary stays legible
  (which `.set()`s cross the wire), the same way `Result` keeps failure legible.
- **Partial failure surfaced**: a synced value carries `Synced | Syncing | Offline(since)` an
  `effect` can render.
- **Identity is light but explicit**: `identity()` = the node's persisted Ed25519 keypair; a topic
  can declare `.members([...]).encrypted()`; default safe, public topic for prototyping.
- **Node lives in the persistent runtime** on its own isolate; configured, not imperatively built.
- Later target: a **synced store** (whole reactive dataset), which is where this and reactive
  persistence (§9.12) merge into one thing. Start per-signal, aim at the store.

Packaging (§9.15.2): **first-party** native extension (trusted, versions with the runtime, links the
internal repr with no host-ABI cost) + **dependency-gated** (enters a build only when the manifest
declares it — whole-feature exclusion, stronger than tree-shaking) + **tree-shaken** within-feature
(DCE prunes unused slices, e.g. no blob transfer; capability-gating reinforces it).

## 2. Where it plugs in — the seams as they exist today

The design's central claim — *"composes cleanly rather than requiring new runtime machinery"* — holds
up against the current code. Every piece it needs already exists:

| Need | Existing seam | File |
| --- | --- | --- |
| A synced value that drives the reactivity graph | `noeta-reactive` is **value-generic** `ReactiveGraph<V>` with the one backend step threaded as a `run` callback; already differential-by-construction | `crates/noeta-reactive/src/lib.rs` |
| Register `synced_signal` + `SyncedSignal<T>` methods, own per-run state, own language values across dispatches, drive the executor, run reactive thunks | The **higher-order ctx seam** — `NativeCtx` (ExtState, `Retained` arena, `spawn_io`/`poll`/`drive`/`advance_tasks`, `run_thunk`/`call_thunk_into`) | `crates/noeta-native/src/ctx.rs` |
| Model it after a real extension | `std.reactive` is **fully migrated** onto that seam: graph = ExtState over `Retained` cells, handles = extern types, gate discipline for fast reads | `crates/noeta-stdlib/src/reactive.rs` |
| Inbound/outbound network as a host capability with a deterministic sandbox + a real backend | The **`Network`** capability trait (pure sandbox responder / request-script vs. reqwest + TcpListener) | `crates/noeta-native/src/host.rs`, `net.rs` |
| Async network events as leaf futures (peer message arrives, sync tick) | The **`ExternIo`** pattern (`NetFetchIo`/`AcceptIo`/`ReplyIo`: `run_sync` in the sandbox, real future on `RealHost`) | `crates/noeta-native/src/net.rs` |
| A long-lived cooperative node loop that never blocks request handling | `http.serve`'s accept→dispatch→reply loop over `NativeCtx` (per-conn tasks reaped cooperatively) | `crates/noeta-stdlib/src/serve.rs` |
| `SyncedSignal`/`Identity`/`SyncStatus` as first-class values | The **extern-type** seam (`ExternValue`: `Response`/`Request`/`Uuid`/`FileHandle`) | `crates/noeta-native/src/net.rs`, extern-types arc |
| The node on its own OS thread, values crossing the boundary safely | **Isolates + `Channel<T>`** copy-at-boundary (`Wire`) | `crates/noeta-vm/src/isolate.rs`, `scheduler.rs` |
| Whole-feature exclusion + within-feature pruning | Manifest dependency gating + **AOT DCE** — *the very branch this is scoped on (`aot-dce`)* implements §9.15.2's tree-shaking prerequisite | `plans/aot/dce.md` |

**New seam required: an 8th Host capability.** p2p is the first capability whose *inbound* events are
neither request/response (http client) nor a finite accept script (http server) but an **open-ended
stream of peer/sync events over time**. It needs its own trait — call it `P2p` — added to the `Host`
union (the `FileSystem + Rng + Clock + Env + Entropy + Ids + Network` supertrait bound), with:

- a **pure deterministic sandbox** driver (a finite, scripted sequence of peer events → then closed,
  exactly like `net_accept_next`'s request script) so the differential oracle still holds, and
- a **`RealHost`** impl wrapping an embedded p2panda node on its own isolate.

This is the same two-implementation shape every capability already has; the novelty is only the
event-stream flavor of the leaf `ExternIo`.

## 3. The hard problem: determinism vs. a differential oracle

The whole test strategy rests on the differential + leak oracles: two backends run the same program
and must agree byte-for-byte, and conformance runs the deterministic sandbox. **Real p2p is
irreducibly non-deterministic** (wall-clock, network timing, peer arrival order, OS entropy for
keys). Everything the design already does for `random`/`clock`/`net`/`entropy` is the template for
resolving this, and it must be followed exactly:

- **Sandbox = a pure, finite, scripted world.** Peer events are a deterministic script the sandbox
  pops (mirroring `net_accept_next`); identity keys come from the fixed-seed `Entropy` stream;
  "sync" is a pure merge the sandbox computes in-process. A synced program **terminates in-oracle**.
- **Real world = `RealHost`-only, never differential-tested** (like reqwest / real disk / real args).
- **The CRDT merge itself is pure and deterministic** and *can* be differential-tested directly —
  that is the correctness-critical core and should carry the heaviest oracle coverage.

This is the single biggest design risk and the thing to prototype first (slice 0 below): confirm a
scripted-peer sandbox produces a stable, meaningful differential before building surface.

## 4. Staging (proposed slices)

Same discipline as reactivity/http (core-first, transport-later, each slice green under both
backends + leak oracle). Rough ordering, not committed:

- **P0 — CRDT core, no network. ✅ DONE.** New dep-free `noeta-crdt` crate (`Mergeable` trait +
  `GCounter`/`PnCounter`/`GSet`, primitive state, immutable values, proptest lattice-law +
  convergence coverage), surfaced as `std.crdt` value extern types (`crdt.{gcounter,pncounter,gset}`
  + `.increment`/`.merge`/… ). Differential (508), leak-0, JIT-differential, and 6 `crdt/`
  conformance cases green. Type-mismatched `merge` is a **static E0007** — a preview of P2's
  compile-time `Mergeable` safety. *No p2panda dependency.* **Deferred to P2:** value-carrying CRDTs
  (LWW-register, OR-Set), which need the retained-arena seam to hold arbitrary language values.
- **P1 — the `P2p` host capability + sandbox script.** Add the 8th capability trait, the pure
  scripted-peer sandbox driver, and the leaf `ExternIo` for "next peer/sync event." Establishes the
  determinism story before any real transport. Validates §3's bet end-to-end on a toy program.
- **P2 — `synced_signal` surface over the ctx seam.** A new stdlib extension crate
  (`std.synced` / fold into `std.reactive`), modeled on `crates/noeta-stdlib/src/reactive.rs`:
  ExtState holds the sync engine over `Retained` cells; `SyncedSignal<T>` / `SyncStatus` extern
  types; a peer event enters as an ordinary reactive `touch` + `flush`. `Mergeable` bound checked at
  `synced_signal(...)` (checker work). Distinct `SyncedSignal<T>` type; status value.
- **P3 — real transport: first-party p2panda extension.** Wrap p2panda in the `RealHost` `P2p`
  impl; embedded node on its own isolate started with the process; identity persisted to disk.
  `noeta add p2p`-style manifest gate so the iroh/QUIC/crypto tree only links when declared.
  CLI-only, not differential.
- **P4 — packaging polish.** DCE within-feature pruning (blob transfer / rendezvous slices) riding
  the `aot-dce` work; capability-gating (`.members().encrypted()` → drop blob/relay machinery when
  ungranted); the Tauri packaging story (§9.9).
- **Later / open.** Synced **store** (the §9.12 merge point); `.history()` / time-travel over the
  append log; group encryption surface.

## 5. Prerequisites & dependencies

- **Bundled server / persistent runtime** — ✅ shipped (http-server arc). The node needs a persistent
  process to live in; it exists.
- **Isolates + channels** — ✅ shipped. The node runs off-thread.
- **AOT DCE** — 🔶 in progress on **this branch** (`aot-dce`, `plans/aot/dce.md`), the §9.15.2
  tree-shaking prerequisite. P4 depends on it; P0–P2 do not.
- **A package manager / manifest dependency gating** — ❌ **not yet built.** The "enters the build
  only when declared" story (frozen/published ABI + dynamic registry) is the open package-manager
  milestone. P3's first-party-but-optional packaging is blocked on it, or must ship as a build
  feature flag in the interim.
- **p2panda maturity** — ⚠️ pre-1.0, APIs not yet stable. The original doc flags this as
  *watch-and-integrate*, not a near-term hard dependency. P0–P2 carry **zero** p2panda dependency by
  design, so all the language-side work (the risky, oracle-sensitive part) proceeds regardless of
  p2panda's timeline; only P3 couples to it.

## 6. Open questions

- Which CRDT types to offer, and how to bound per-value metadata overhead (the doc's own open Q).
- Per-signal vs. per-store granularity — start per-signal, but the store form is where this fuses
  with reactive persistence; decide before the surface ossifies.
- Does `Mergeable` checking need new checker machinery, or does the existing bounded-generics /
  trait-coherence surface cover it?
- Is `std.synced` a new virtual module or an extension of `std.reactive`? (The graph is already the
  same machinery; a synced signal is a signal whose `set` also emits to the network and whose
  network events call `touch`.)
- Sandbox peer-script format — how a conformance fixture declares "peer B sends this op at step 3."

## 7. Recommendation

The design's core bet is sound and the seams confirm it: **no new runtime machinery is required for
P0–P2** — the reactivity graph, the ctx seam, the host-capability pattern, extern types, and isolates
all already carry exactly the shape this needs, and the one genuinely new seam (the `P2p` capability)
is a well-worn two-implementation pattern.

The right first move is **P0 + P1** — the pure CRDT core and the deterministic scripted-peer sandbox
capability. They are fully oracle-testable, carry no external dependency, and de-risk the one hard
question (§3: can a scripted-peer sandbox give a stable differential?) before any surface or transport
is committed. Real transport (P3) should stay parked until the package-manager milestone lands and
p2panda stabilizes; that gate is a packaging concern, not a blocker for proving the model.
