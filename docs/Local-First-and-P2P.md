# Local-First & Peer-to-Peer State

The standard library ships the building blocks for **local-first, collaborative state** (architecture §9.15): conflict-free replicated data types (**CRDTs**), a **peer-to-peer transport**, and — where they meet — a **synced signal**: reactive state that several peers edit concurrently and that converges without coordination. A peer's change flows into the [reactivity graph](Reactivity) and reruns your `computed`/`effect`s exactly like a local edit — *signals that happen to be shared*.

```noeta
use std.{crdt}
use std.reactive.{effect}
use std.synced.{synced_signal}

// Two replicas of the same counter, on one topic.
a = synced_signal(crdt.gcounter().increment("A", 1), "counter")
b = synced_signal(crdt.gcounter().increment("B", 2), "counter")

// An effect observes replica `a` reactively.
effect(fn() { echo "a = ${a.get().value()}" })   // prints "a = 1"

a.sync()   // pull peers' state → a merges b's → "a = 3" (the effect reruns)
b.sync()   // b merges a's state → also 3

echo "converged: ${a.get().value()} == ${b.get().value()}"   // 3 == 3
```

> **Status.** The data-convergence and language layers are complete and run entirely locally today (a single-node, in-process loopback that models peers deterministically). The real networked transport — peer discovery, NAT traversal, gossip — is a later milestone (see [What's next](#whats-next)). This is a supported, opt-in path, not something imposed on every program.

## CRDTs — `std.crdt`

A CRDT is a value whose concurrent edits **merge** into the same result regardless of the order they arrive in. `merge` is commutative, associative, and idempotent, so replicas converge without a coordinator — and duplicated or out-of-order messages are harmless. CRDTs are ordinary immutable values (an update returns a new value); they compare by content and print for debugging.

Each carries a **replica id** — a string identifying the node that made a change — which you supply explicitly.

| Constructor | Type | What it is |
| --- | --- | --- |
| `crdt.gcounter()` | `GCounter` | A grow-only counter (only increments). |
| `crdt.pncounter()` | `PnCounter` | A counter that also decrements. |
| `crdt.gset()` | `GSet` | A grow-only set of strings. |

```noeta
use std.{crdt}

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
use std.{crdt}
a = crdt.gcounter()
b = crdt.gset()
c = a.merge(b)   // compile error: argument of type `GSet` is not assignable to `GCounter`
```

## Peer-to-peer messaging — `std.p2p`

`std.p2p` is the transport underneath synced state: publish a message to a **topic**, receive messages other peers published to it. Messages are opaque bytes (a string rides as its UTF-8), so any payload — including serialized CRDT state — travels over it.

```noeta
use std.{p2p}

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

Topics are independent broadcast channels: every subscriber sees every message, and receiving from an empty topic yields `none` immediately.

## Synced signals — `std.synced`

A `synced_signal(initial, topic)` fuses the two: a reactive [signal](Reactivity) whose value is a CRDT and whose changes are shared over a p2p topic. Its value type must be `Mergeable` — i.e. a CRDT — which the compiler enforces, so you can never accidentally sync a value with no convergence story:

```noeta error
use std.synced.{synced_signal}
synced_signal(42, "counter")   // compile error: `int` does not satisfy the bound `Mergeable`
```

The surface is a signal you converge rather than overwrite:

- `.get(): T` — the current merged value. Read inside a `computed`/`effect` to subscribe to it.
- `.merge(delta: T)` — merge `delta` into the local value, rerun dependents, and **publish** the new state to peers.
- `.sync()` — **pull**: drain the topic, merge every peer's state in, and rerun dependents once if anything changed.

`.sync()` is deliberately explicit — the network boundary stays visible, so it is legible in your code exactly where remote state enters, rather than hiding behind every read.

```noeta
use std.{crdt}
use std.synced.{synced_signal}

// A shared set of who's online, replicated on the "presence" topic.
here = synced_signal(crdt.gset(), "presence")
here.merge(crdt.gset().insert("alice"))   // announce alice — and broadcast

// ...another peer announces "bob" on the same topic...

here.sync()                                // pull peers in
echo here.get().members()                  // ["alice", "bob"] — converged
```

Because a synced signal is an ordinary node in the reactivity graph, everything reactive composes with it: a `computed` derived from `here.get()` recomputes when a peer joins, an `effect` re-renders, and a diamond of dependencies still settles glitch-free.

## What's next

Two pieces complete the story and are tracked as future milestones:

- **Real networked transport.** Today the transport is an in-process broker: it models peers deterministically and runs a synced program end-to-end on one node, but it does not yet cross the network. The planned backing is [p2panda](https://p2panda.org) — discovery (mDNS + rendezvous), NAT-traversing QUIC transport, gossip, and encryption — shipped as an opt-in first-party extension so a program that does not use it pays nothing for it. It arrives once the package manager and that extension land.
- **Richer collaborative types.** A last-write-wins register and an add/remove set (OR-Set) — CRDTs that carry arbitrary application values, not just counters and string sets — plus a per-value sync **status** (`Synced` / `Syncing` / `Offline`) that a real transport makes meaningful.

See also [Reactivity](Reactivity) for the signal/computed/effect core these build on, and [Standard-Library Modules](Standard-Library-Modules) for the full module surface.
