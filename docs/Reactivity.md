# Reactivity

The standard library ships **fine-grained reactivity** — `signal`, `computed`, and `effect` — the same model SolidJS popularized, running server-side. State lives in signals; derivations and side effects declare what they read, and the runtime reruns exactly what a change affects, in a deterministic, glitch-free order. These are ordinary stdlib values (no new keywords), the load-bearing primitive behind the reactive-single-binary story (architecture §9.4); the transport layer that pushes changes to a browser is a later milestone (see [What's next](#whats-next)).

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

Setting to a value equal to the current one still fires dependents — a change is a `set`, not a value difference (there is no equality suppression; it would not always be cheap, and this keeps the contract simple).

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

The rerun order is deterministic — ascending creation order within a flush round — so both execution backends produce byte-identical output (the differential oracle checks this on every program).

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

Every reactive node belongs to the program's implicit scope; all of them are reclaimed when the program ends. For finer control, an `effect` is **disposable** via `.dispose()`, which severs its subscriptions so it stops rerunning. Signals and computeds are not independently disposable — a computed has no side effect to stop, and both are freed with the scope. (Nested reactive scopes, where an effect owns and disposes child effects it creates, are a later addition.)

## What's next

This is the reactive **core**. The layers that consume it — WebSocket minimal-diff push / LiveView-style UI, reactive persistence, hot-module reload, and CRDT-synced signals — are deferred to when their prerequisites (the bundled HTTP/WS server, the DB layer, the p2p stack) land. The core already exposes which computeds changed in a flush, the hook the diff layer will build on.
