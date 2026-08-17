# Reactivity

The standard library ships **fine-grained reactivity** — `signal`, `computed`, and `effect` — the same model SolidJS popularized, running server-side. State lives in signals; derivations and side effects declare what they read, and the runtime reruns exactly what a change affects, in a deterministic, glitch-free order. These are ordinary stdlib values (no new keywords); the transport that carries changes to a browser — the view/diff protocol over the bundled WebSocket server — is covered under [Views](#views--pushing-state-to-a-client) below.

```noeta
use std.reactive.{signal, computed, effect}
count = signal(0)
doubled = computed(fn() => count.get() * 2)
effect(fn() { echo "count is ${count.get()}, doubled is ${doubled.get()}" })

count.set(5)          // reruns the effect once: "count is 5, doubled is 10"
```

## Signals — mutable reactive state

`signal(initial)` creates a cell of type `Signal<T>`. It carries its value type statically, so reads and writes are type-checked.

- `.get(): T` — read the current value (and, inside a `computed`/`effect` body, subscribe to it).
- `.set(v: T)` — replace the value and rerun whatever depends on it.
- `.update(fn(T) => T)` — read-modify-write convenience.

```noeta
use std.reactive.{signal}
n = signal(1)
echo n.get()                    // 1
n.set(10)
echo n.get()                    // 10
n.update(fn(x) => x + 5)
echo n.get()                    // 15
```

By default, setting a signal to a value equal to the current one **still fires** dependents — a change is a `set`, not a value difference, which keeps the default contract simple (and equality is not always cheap to compute).

### Opt-in equality suppression

Pass `signal(initial, dedupe: true)` — a trailing `bool` flag — to make a signal skip re-firing when a `set`/`update` lands a value **equal to the current one under the language's `==`**. The comparison is exactly the `==` operator: structural for value types and collections (two lists with equal elements are equal), reference identity for a `class` without `impl Equatable`. It applies **only** to signals created with the flag; a plain `signal(initial)` is unchanged.

```noeta
use std.reactive.{signal, effect}
count = signal(0, true)                 // dedupe on
effect(fn() { echo "count is ${count.get()}" })   // prints once: "count is 0"
count.set(0)                            // equal → suppressed, no rerun
count.set(1)                            // changed → reruns: "count is 1"

items = signal([1, 2, 3], true)
items.set([1, 2, 3])                    // structurally equal → suppressed
items.set([1, 2, 3, 4])                 // different → reruns
```

Use it for signals fed by a source that re-emits the same value (a poll, a recompute, an incoming message), so downstream effects only run on genuine change. Leave it off — the default — when every `set` should be observed, or when `==` would be expensive relative to the work a rerun does.

## Computeds — lazy, memoized derivations

`computed(fn() => T)` creates a `Computed<T>`: a derivation that recomputes **only when a dependency changed**, and returns its memo otherwise. It is **lazy** — the body does not run until the first `.get()` — and **read-only** (there is no `.set()`).

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

A `computed` may read other computeds; a chain recomputes transitively, each level memoizing. Calling `.set()` on a computed is rejected with `E0005` (it has no such method — its value is derived, not assigned).

## Effects — eager side effects

`effect(fn() => void)` runs its body **immediately**, tracks the signals and computeds it read, and **reruns** whenever one of them changes. It returns an `Effect` handle whose only method is `.dispose()`.

```noeta
use std.reactive.{signal, effect}
temp = signal(20)
watcher = effect(fn() { echo "temperature is ${temp.get()}" })
// prints "temperature is 20" right away

temp.set(25)                    // reruns: "temperature is 25"
watcher.dispose()               // unsubscribe — stops reacting
temp.set(30)                    // no rerun; temp.get() still returns 30
```

An `Effect` has no readable value — `.get()` on one is `E0005`.

## Dependency tracking is automatic and dynamic

A `.get()` inside a running `computed`/`effect` body subscribes that body to what it read; a `.get()` in ordinary code (outside any body) is a plain read that subscribes nothing. Dependencies are recaptured on **every** run, so a body that reads different signals on different runs tracks exactly the ones it read last:

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

When one signal feeds several derivations that feed a common consumer (a diamond), a single `set` reruns the consumer **once**, seeing a consistent set of inputs — never a half-updated mix. This falls out of the lazy-pull model: a dirty computed is always forced fresh on read.

```noeta
use std.reactive.{signal, computed, effect}
base  = signal(2)
plus  = computed(fn() => base.get() + 10)
times = computed(fn() => base.get() * 10)
effect(fn() { echo "sum is ${plus.get() + times.get()}" })

base.set(3)       // effect reruns exactly once: "sum is 43" (13 + 30)
```

The rerun order is deterministic — ascending creation order within a flush round (verified across backends by the [differential oracle](Architecture-and-Pipeline#the-two-backend-differential-oracle)).

## Batching and coalescing

A `set`/`update` performed **inside** a running effect does not start a nested update — it folds into the flush already in progress, which runs to a fixpoint. So an effect that writes a signal another effect reads drives that second effect within the same flush, once, in order.

## Non-termination is caught, not hung

An effect that changes a signal it depends on would rerun forever. Rather than hang, the scheduler bounds each flush and raises **`E0045` ReactiveCycle** once it exceeds the step limit:

```noeta error
use std.reactive.{signal, effect}
n = signal(0)
effect(fn() { n.set(n.get() + 1) })   // reads n and writes n — never settles
// → E0045: reactive update did not converge
```

## Disposal and ownership

An `effect` is **disposable** via `.dispose()`, which severs its subscriptions so it stops rerunning. Signals and computeds are not independently disposable — a computed has no side effect to stop, and both are freed by their owner or the scope.

Ownership is a **tree** (the SolidJS model): a reactive node created *while a `computed`/`effect` body is running* is **owned** by that body. When the owner reruns — or is itself disposed — it disposes the children it created on its previous run (and their children, recursively) **before** running again. So a body that creates reactive nodes on every run does not accumulate duplicated effects:

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

Without this, each `outer` run would leave a live child subscribed to `dep`, and `dep.set` would fire an ever-growing pile of stale copies. A child's backing cells are reclaimed the moment its owner tears it down, not merely at program end. Nodes created at the top level (outside any body) are roots, reclaimed when the program ends; a foreign reactive source (a CRDT-synced signal, a reactive DB query) is always a root — it owns its own lifetime and is never torn down by a rerun of whatever body happened to construct it.

## Views — pushing state to a client

A `view()` is a named window onto reactive state, built for pushing changes over a wire (the LiveView pattern — see [std.http](std-http)). `expose(name, handle)` binds a name to a `Signal`, `Computed`, or `SyncedSignal`; `snapshot()` renders the full state as a JSON frame (and baselines the view); `diff()` renders a frame of **only the bindings whose value changed since** — or `none` when nothing observably changed:

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

Minimality is enforced twice: the flush records exactly which nodes changed (the set signal plus computeds it transitively dirtied), so `diff()` never inspects untouched bindings; and each candidate's fresh value is compared against the last one pushed, so a write of an equal value — or a recompute that lands on the same result — pushes nothing. Frames are deterministic (name-sorted keys, the `json.stringify` encoding), so a scripted client conversation pins them byte-exactly in tests — and each binding is written from the type it was exposed under, so a `Signal<u64>` pushes the same digits `json.stringify` would give it (see [Fixed-Width Ints](Fixed-Width-Integers)). The change tracking is pay-for-use: until the first `view()` exists, a hot `set` loop records nothing.

Typically each websocket session creates its own view and sends `snapshot()` on connect and `diff()` after handling each client event — the bundled browser shim (`server.liveview_js()`) applies those frames to the DOM. The wiring, as a sketch (the complete runnable version is `examples/liveview_counter.noe`):

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

This page is the reactive **core**. Three surfaces consume it:

- **CRDT-synced signals** — reactive state several peers edit concurrently, converging without coordination (the `para/p2p` package, github.com/noeta-lang/para-p2p).
- **The view/diff push protocol** above, which carries signal changes to a browser over the bundled WebSocket server.
- **LiveView** — server-side reactive HTML via `@html` templates whose holes are signals, built on this graph and that transport (the `para/html` package, github.com/noeta-lang/para-html).

Signals also survive code edits under [`noeta serve --watch`](The-CLI#noeta-serve-and---watch): an unchanged `signal(...)` binding keeps its value across a hot swap, while plain top-level bindings re-initialize.
