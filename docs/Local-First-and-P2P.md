# Local-First & Peer-to-Peer State

The **`para-p2p` package** provides the building blocks for **local-first, collaborative state** (architecture §9.15): conflict-free replicated data types (**CRDTs**), a **peer-to-peer transport**, and — where they meet — a **synced signal**: reactive state that several peers edit concurrently and that converges without coordination. A peer's change flows into the [reactivity graph](Reactivity) and reruns your `computed`/`effect`s exactly like a local edit — *signals that happen to be shared*.

It is a **first-party but non-default** package under the `para` ("alongside") namespace — maintained by the project, but not part of the always-on `std` stdlib, so a program that never needs peer-to-peer state carries none of its weight. Add it to your `noeta.toml` and, because it ships native code, authorize it in `[trust]`:

```toml
[dependencies]
para = { path = "…/packages/para-p2p" }   # or a registry/git version once published

[trust]
native = ["para/p2p"]
```

Its modules then resolve as `para.crdt`, `para.p2p`, and `para.synced`. (The [LiveView](LiveView) `para.html` package is its sibling under the same namespace.)

```noeta check
use para.{crdt}
use std.reactive.{effect}
use para.synced.{synced_signal}

// Two replicas of the same counter, on one topic.
a = synced_signal(crdt.gcounter().increment("A", 1), "counter")
b = synced_signal(crdt.gcounter().increment("B", 2), "counter")

// An effect observes replica `a` reactively.
effect(fn() { echo "a = ${a.get().value()}" })   // prints "a = 1"

a.sync()   // pull peers' state → a merges b's → "a = 3" (the effect reruns)
b.sync()   // b merges a's state → also 3

echo "converged: ${a.get().value()} == ${b.get().value()}"   // 3 == 3
```

> **Status.** All three layers ship today. Data convergence and the language surface run entirely locally on a deterministic in-process loopback that models peers; and a real networked transport — [p2panda](https://p2panda.org) peer discovery (mDNS), NAT-traversing QUIC, gossip + eventual-consistency log-sync, a persistent per-node identity, and end-to-end **group encryption** — backs `noeta run` under the default `ring-p2p` build. A program that never syncs pays nothing for it: the whole p2panda tree is dropped from a tailored `noeta build --native` binary that imports neither `para.p2p` nor `para.synced`. This is a supported, opt-in path, not something imposed on every program.

## CRDTs — `para.crdt`

A CRDT is a value whose concurrent edits **merge** into the same result regardless of the order they arrive in. `merge` is commutative, associative, and idempotent, so replicas converge without a coordinator — and duplicated or out-of-order messages are harmless. CRDTs are ordinary immutable values (an update returns a new value); they compare by content and print for debugging.

Each carries a **replica id** — a string identifying the node that made a change — which you supply explicitly.

| Constructor | Type | What it is |
| --- | --- | --- |
| `crdt.gcounter()` | `GCounter` | A grow-only counter (only increments). |
| `crdt.pncounter()` | `PnCounter` | A counter that also decrements. |
| `crdt.gset()` | `GSet` | A grow-only set of strings. |

```noeta check
use para.{crdt}

// A grow-only counter: each replica accumulates its own count; merge takes the per-replica max,
// so two replicas that incremented independently converge to the total.
a = crdt.gcounter().increment("A", 3)      // increment(replica, by = 1)
b = crdt.gcounter().increment("B", 4)
echo a.merge(b).value()                     // 7  — and a.merge(b) == b.merge(a)

// A PN-counter nets increments against decrements and may go negative.
c = crdt.pncounter().increment("A", 10).decrement("A", 3)
echo c.value()                              // 7

// A grow-only set converges by union; members come back sorted.
s = crdt.gset().insert("x").insert("y").merge(crdt.gset().insert("z"))
has_z = s.contains("z")
echo "${s.members()} has_z=${has_z}"             // ["x", "y", "z"] has_z=true
```

**Methods.** Every CRDT has `.merge(other)` (returning the converged value) and a reader: `GCounter`/`PnCounter` expose `.value(): int`; `GSet` exposes `.contains(e): bool`, `.len(): int`, and `.members(): [string]`. Counters take `.increment(replica, by=1)` (and `PnCounter` also `.decrement(replica, by=1)`); a grow-only counter rejects a negative amount — use a `PnCounter` when you need to go down. `.merge` only accepts the *same* CRDT type, checked statically:

```noeta error
use para.{crdt}
a = crdt.gcounter()
b = crdt.gset()
c = a.merge(b)   // compile error: argument of type `GSet` is not assignable to `GCounter`
```

## Peer-to-peer messaging — `para.p2p`

`para.p2p` is the transport underneath synced state: publish a message to a **topic**, receive messages other peers published to it. Messages are opaque bytes (a string rides as its UTF-8), so any payload — including serialized CRDT state — travels over it.

```noeta check
use para.{p2p}

async fn drain(): void {
    p2p.publish("room", "hello")
    p2p.publish("room", "world")
    mut running = true
    while running {
        msg = p2p.receive("room").await          // Future<?bytes> — none once drained
        (hex, keep) = match msg {
            some(bytes) => (bytes.to_hex(), true),
            none => ("", false),
        }
        if keep { echo hex }
        running = keep
    }
}
```

- `p2p.publish(topic, message: string | bytes)` — broadcast to everyone on the topic.
- `p2p.receive(topic): Future<?bytes>` — the next message (`await` it); `none` once there is nothing more.
- `p2p.identity(): ?string` — this node's stable identity (the hex Ed25519 public key it signs with); `none` under the sandbox, which has no network identity.

Topics are independent broadcast channels: every subscriber sees every message, and receiving from an empty topic yields `none` immediately.

## Synced signals — `para.synced`

A `synced_signal(initial, topic)` fuses the two: a reactive [signal](Reactivity) whose value is a CRDT and whose changes are shared over a p2p topic. Its value type must be `Mergeable` — i.e. a CRDT — which the compiler enforces, so you can never accidentally sync a value with no convergence story:

```noeta error
use para.synced.{synced_signal}
synced_signal(42, "counter")   // compile error: `int` does not satisfy the bound `Mergeable`
```

The surface is a signal you converge rather than overwrite:

- `.get(): T` — the current merged value. Read inside a `computed`/`effect` to subscribe to it.
- `.merge(delta: T)` — merge `delta` into the local value, rerun dependents, and **publish** the new state to peers.
- `.sync()` — **pull**: drain the topic, merge every peer's state in, and rerun dependents once if anything changed.

`.sync()` is deliberately explicit — the network boundary stays visible, so it is legible in your code exactly where remote state enters, rather than hiding behind every read.

```noeta check
use para.{crdt}
use para.synced.{synced_signal}

// A shared set of who's online, replicated on the "presence" topic.
here = synced_signal(crdt.gset(), "presence")
here.merge(crdt.gset().insert("alice"))   // announce alice — and broadcast

// ...another peer announces "bob" on the same topic...

here.sync()                                // pull peers in
echo here.get().members()                  // ["alice", "bob"] — converged
```

Because a synced signal is an ordinary node in the reactivity graph, everything reactive composes with it: a `computed` derived from `here.get()` recomputes when a peer joins, an `effect` re-renders, and a diamond of dependencies still settles glitch-free.

### Encrypted groups

Add a third argument — a **member set** — to make a synced signal end-to-end encrypted to exactly those peers. Every state it publishes crosses the wire encrypted; a node outside the set sees only ciphertext it cannot read.

```noeta check
use para.{crdt}
use para.synced.{synced_signal}

// A shared tally readable only by alice and bob (peer ids are their node identities).
tally = synced_signal(crdt.gcounter(), "team/tally", [alice_id, bob_id])
tally.merge(crdt.gcounter().increment("alice", 1))   // encrypted before it leaves the node
tally.sync()                                          // decrypts peers' state in
```

The members list is given at construction, so the very **first** state announced to the topic is already encrypted — there is no window where it goes out in the clear. Encryption is transparent to your program: `.get()` returns the same converged value it would without it, so the deterministic sandbox treats an encrypted signal as a pass-through and a synced program behaves identically whether or not it is encrypted. Under the real transport it is backed by [p2panda-spaces](https://p2panda.org) group encryption (a symmetric group key with post-compromise security; a member that joins late can still decrypt prior state — exactly what a convergent CRDT needs). Membership uses the group's declared set: the elected creator welcomes each member as it announces its key on the topic, with no out-of-band key exchange.

Membership is not fixed for life — you can change it at runtime:

- `.add_member(peer_id)` — admit a peer to the group (welcomed once its key arrives on the topic).
- `.remove_member(peer_id)` — remove a peer and **rotate the group key**, so the removed peer can no longer decrypt state published afterward (revocation).

```noeta check
tally.add_member(carol_id)      // carol can now read new state
tally.remove_member(bob_id)     // bob is revoked — the key rotates; he can't read anything published now on
```

The group creator is authoritative over membership (it holds the manage capability). Membership is transparent to the converged value, so these are no-ops under the sandbox and real only over the live transport.

A per-signal **`.status(): string`** — `"synced"` / `"syncing"` / `"offline"` — reports this replica's convergence state against its peers (always `"synced"` on the single-node sandbox; meaningful over a real network, e.g. to render "working offline").

## What's next

The transport, encryption, identity, and dynamic-membership layers ship today. Remaining future milestones:

- **Richer collaborative types.** A last-write-wins register and an add/remove set (OR-Set) — CRDTs that carry arbitrary application values, not just counters and string sets.

See also [Reactivity](Reactivity) for the signal/computed/effect core these build on, and [Standard-Library Modules](Standard-Library-Modules) for the full module surface.
