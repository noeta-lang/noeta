# Reactivity

The standard library ships **fine-grained reactivity** as three ordinary values: `signal`, `computed`, and `effect`. State lives in signals; derivations and side effects declare what they read by reading it, and the runtime reruns exactly what a change affects, in a deterministic, glitch-free order. It is the model SolidJS popularized, running server-side, and it needs no new keywords.

```noeta
use std.reactive.{signal, computed, effect}
count = signal(0)
doubled = computed(fn() => count.get() * 2)
effect(fn() { echo "count is ${count.get()}, doubled is ${doubled.get()}" })

count.set(5)          // reruns the effect once: "count is 5, doubled is 10"
```

## Signals

`signal(initial)` creates a cell of type `Signal<T>`. It carries its value type statically, so reads and writes are type-checked.

| Method | Signature | What it does |
|---|---|---|
| `get` | `get() -> T` | read the current value, and subscribe to it inside a `computed`/`effect` body |
| `set` | `set(v: T)` | replace the value and rerun whatever depends on it |
| `update` | `update(f: Fn(T) -> T)` | read-modify-write in one call |

```noeta
use std.reactive.{signal}
n = signal(1)
echo n.get()                    // 1
n.set(10)
echo n.get()                    // 10
n.update(fn(x) => x + 5)
echo n.get()                    // 15
```

A `set` fires dependents whether or not the new value equals the old one, so the write itself is the change.

### Opt-in equality suppression

`signal(initial, dedupe: true)` makes a signal skip re-firing when a `set` or `update` lands a value **equal to the current one under the language's `==`**: structural for value types and collections, so two lists with equal elements are equal, and reference identity for a `class` without `impl Equatable`. The flag applies to the signal it was passed to, and a plain `signal(initial)` keeps the always-fire behavior.

```noeta
use std.reactive.{signal, effect}
count = signal(0, dedupe: true)
effect(fn() { echo "count is ${count.get()}" })   // prints once: "count is 0"
count.set(0)                            // equal, so suppressed: no rerun
count.set(1)                            // changed, so reruns: "count is 1"

items = signal([1, 2, 3], dedupe: true)
items.set([1, 2, 3])                    // structurally equal, suppressed
items.set([1, 2, 3, 4])                 // different, reruns
```

Reach for it when a signal is fed by a source that re-emits the same value, such as a poll, a recompute, or an incoming message, so downstream effects run on genuine change.

## Computeds

`computed(fn() => T)` creates a `Computed<T>`, a derivation that recomputes when a dependency changed and returns its memo otherwise. It is **lazy**: the body first runs at the first `.get()`. Its one method is `.get()`.

```noeta
use std.reactive.{signal, computed}
first = signal("Ada")
last  = signal("Lovelace")
full  = computed(fn() => "${first.get()} ${last.get()}")

echo full.get()                 // "Ada Lovelace" — computes now
echo full.get()                 // memoized — does not recompute
first.set("Grace")
echo full.get()                 // "Grace Lovelace" — recomputes once
```

A `computed` may read other computeds, and a chain recomputes transitively with each level memoizing. A `Computed` is read-only, so `.set()` on one is `E0005`, the error any value gives for a method it does not carry.

## Effects

`effect(f)` runs `f` **immediately**, tracks the signals and computeds it read, and **reruns** whenever one of them changes. The body's return value is discarded. It hands back an `Effect` handle whose one method is `.dispose()`, and `.get()` on one is `E0005`.

```noeta
use std.reactive.{signal, effect}
temp = signal(20)
watcher = effect(fn() { echo "temperature is ${temp.get()}" })
// prints "temperature is 20" right away

temp.set(25)                    // reruns: "temperature is 25"
watcher.dispose()               // unsubscribe, stops reacting
temp.set(30)                    // no rerun; temp.get() still returns 30
```

## Dependency tracking is automatic and dynamic

A `.get()` inside a running `computed`/`effect` body subscribes that body to what it read. A `.get()` in ordinary code, outside any body, is a plain read that subscribes nothing. Dependencies are recaptured on **every** run, so a body that reads different signals on different runs tracks exactly the ones it read last:

```noeta
use std.reactive.{signal, effect}
useLeft = signal(true)
left  = signal(1)
right = signal(2)
effect(fn() {
    echo "value is ${if useLeft.get() then left.get() else right.get()}"
})
right.set(20)     // no rerun while useLeft is true — `right` was not read
useLeft.set(false) // reruns, now reading `right`; `left` is unsubscribed
```

## Glitch-free by construction

When one signal feeds several derivations that feed a common consumer (a diamond), a single `set` reruns the consumer **once**, over a consistent set of inputs. That falls out of the lazy-pull model, where a dirty computed is always forced fresh on read.

```noeta
use std.reactive.{signal, computed, effect}
base  = signal(2)
plus  = computed(fn() => base.get() + 10)
times = computed(fn() => base.get() * 10)
effect(fn() { echo "sum is ${plus.get() + times.get()}" })

base.set(3)       // effect reruns exactly once: "sum is 43" (13 + 30)
```

The rerun order is deterministic: ascending creation order within a flush round.

## Batching and coalescing

A `set` or `update` performed **inside** a running effect folds into the flush already in progress, which runs to a fixpoint. An effect that writes a signal another effect reads therefore drives that second effect within the same flush, once, in order.

## A flush that does not converge raises E0045

An effect that changes a signal it depends on would rerun forever. The scheduler bounds each flush and raises **`E0045` ReactiveCycle** once it exceeds the step limit:

```noeta error
use std.reactive.{signal, effect}
n = signal(0)
effect(fn() { n.set(n.get() + 1) })   // reads n and writes n — never settles
// → E0045: reactive update did not converge
```

## Disposal and ownership

`.dispose()` on an `effect` severs its subscriptions so it stops rerunning. Signals and computeds are freed by their owner or their scope instead, since a computed has no side effect to stop.

Ownership is a **tree**, the SolidJS model: a reactive node created *while a `computed`/`effect` body is running* is **owned** by that body. When the owner reruns, or is itself disposed, it disposes the children it created on its previous run, and their children recursively, **before** running again. A body that creates reactive nodes on every run therefore keeps exactly one live copy of each:

```noeta
use std.reactive.{signal, effect}
outer = signal(0)
dep   = signal(0)
effect(fn() {
    outer.get()
    effect(fn() { echo "dep is ${dep.get()}" })   // a child effect, owned by the outer one
})
outer.set(1)          // reruns outer: last run's child is disposed, a fresh one created
dep.set(5)            // exactly ONE live child reacts — not one per outer run
```

A child's backing cells are reclaimed the moment its owner tears it down. Nodes created at the top level, outside any body, are roots, reclaimed when the program ends. A foreign reactive source, such as a CRDT-synced signal or a reactive DB query, is always a root: it owns its own lifetime, and a rerun of whatever body constructed it never tears it down.

## Views

A `view()` is a named window onto reactive state, built for pushing changes over a wire (the LiveView pattern, see [std.http](std-http)).

| Method | Signature | What it does |
|---|---|---|
| `expose` | `expose(name: string, handle: dyn)` | bind `name` to a `Signal`, `Computed`, or `SyncedSignal`; re-exposing a name replaces its binding |
| `unexpose` | `unexpose(name: string)` | drop the binding and dispose its handle, so a diff never pushes it again |
| `snapshot` | `snapshot() -> string` | render the full state as a JSON frame, and baseline the view |
| `diff` | `diff() -> ?string` | render a frame of only the bindings whose value changed since, or `none` |

```noeta
use std.reactive.{signal, computed, view}

count = signal(2)
double = computed(fn() { return count.get() * 2 })

v = view()
v.expose("count", count)
v.expose("double", double)

echo v.snapshot()        // {"type":"snapshot","values":{"count":2,"double":4}}
count.set(3)
echo v.diff() ?? "none"  // {"type":"patch","changes":{"count":3,"double":6}}
count.set(3)
echo v.diff() ?? "none"  // none — same value, nothing to push
```

Minimality is enforced twice. The flush records which nodes changed, meaning the set signal plus the computeds it transitively dirtied, so `diff()` never inspects untouched bindings. Each candidate's fresh value is then compared against the last one pushed, so a write of an equal value, or a recompute landing on the same result, pushes nothing.

Frames are deterministic, with name-sorted keys and the `json.stringify` encoding, and each binding is written from the type it was exposed under, so a `Signal<u64>` pushes the same digits `json.stringify` would give it (see [Fixed-Width Ints](Fixed-Width-Integers)). Change tracking is pay-for-use: until the first `view()` exists, a hot `set` loop records nothing.

Typically each websocket session creates its own view and sends `snapshot()` on connect and `diff()` after handling each client event, and the bundled browser shim (`server.liveview_js()`) applies those frames to the DOM. The wiring, as a sketch (the complete runnable version is `examples/liveview_counter.noe`):

```noeta ignore
// sketch — the session loop of a LiveView server (`noeta serve app.noe`)
async fn session(sock: Socket) use (count, double): bool {
    v = view()
    v.expose("count", count)
    v.expose("double", double)
    sock.send(v.snapshot())                  // full state on connect
    while true {
        msg = sock.recv().await
        if msg == none { return true }       // client hung up
        // …handle the event: count.update(…), count.set(0), …
        patch = v.diff() ?? ""
        if patch != "" { sock.send(patch) }  // push only what changed
    }
}

fn fetch(req: Request): Response {
    if req.path() == "/ws" { return server.websocket(session) }
    return server.response(200, page(), {"content-type": "text/html"})
}
```

## Layers built on this graph

Three surfaces consume the reactive core:

- **CRDT-synced signals**, reactive state several peers edit concurrently, converging without coordination (the `para/p2p` package, github.com/noeta-lang/para-p2p).
- **The view/diff push protocol** above, which carries signal changes to a browser over the bundled WebSocket server.
- **LiveView**, server-side reactive HTML via `@html` templates whose holes are signals, built on this graph and that transport (the `para/html` package, github.com/noeta-lang/para-html).

Signals also survive code edits under [`noeta serve --watch`](The-CLI#noeta-serve-and---watch): an unchanged `signal(...)` binding keeps its value across a hot swap, while plain top-level bindings re-initialize.
