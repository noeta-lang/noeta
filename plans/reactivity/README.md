# Reactivity — server-side signals (`signal` / `computed` / `effect`)

**Status: planning (proposal for sign-off).** This is the first planning pass on the M2 differentiator
drafted in architecture §9.4. It scopes the **reactive core only** — the signal/computed/effect graph
with automatic dependency tracking, deterministic scheduling, and disposal — built as a shared runtime
component both backends drive. The transport half (WebSocket minimal-diff push, the LiveView story) is
**deferred to when the bundled HTTP/WS server lands**; this milestone delivers the load-bearing primitive
that server, reactive persistence (§9.12), HMR (§9.14), and synced/CRDT state (§9.15) all reuse.

## Why now / why this shape

Signals are the keystone the "reactive single-binary" positioning rests on — the draft reuses them four
times. But the §9.4 draft is three bullet points; the language design is unspecified. The realization
that makes this a clean next milestone: **the reactive core is a self-contained runtime component with
zero I/O**, so it fits our oracle discipline exactly the way `Host`, channels, and isolates did — one
shared core both backends drive, differential-identical by construction. We build the credible version
(a correct, tested reactivity graph) now; the unforgettable version (WS diff-push) rides on top later —
the same staging discipline the draft preaches everywhere.

## Decisions locked (with the user)

1. **Surface = stdlib values, not keywords.** `signal(0)`, `computed(fn() => …)`, `effect(fn() => …)`
   are ordinary stdlib functions returning heap value types. No new keywords, no parser/checker surface.
   Matches §7.2's "the language owns nothing more" ethos — the same call we made for `TaskScope`.
2. **Scope = reactive core only.** Graph + dependency tracking + scheduling + disposal, fully
   oracle-covered (differential + leak). WS transport / diff-push deferred to the server milestone.

## Decisions proposed (confirm before S1)

3. **Evaluation model = lazy-memo (SolidJS model).** `computed` is lazy: it recomputes on read only when
   a dependency has changed (dirty), and memoizes otherwise. `effect` is eager: it runs once on creation
   and reruns when a dependency changes. This is the proven correct-and-efficient model and is fully
   deterministic (no wall-clock, no thread scheduling).
4. **Scheduling = deterministic topological flush, batched.** A `.set()` marks dependents dirty and
   enqueues affected effects; the flush walks them in **topological order** with an insertion-order
   tie-break, so a diamond dependency (A→B, A→C, B&C→D) recomputes D **once**, glitch-free, in an order
   both backends reproduce byte-for-byte. Nested `.set()`s inside an effect coalesce into the current
   flush. This determinism is the oracle's proof obligation — it is what lets `--differential` see the
   whole feature.
5. **Disposal = scope-owned, mirroring `TaskScope`.** A live `effect` holds subscriptions and must be
   disposable or it leaks (our leak oracle catches it immediately). Effects are owned by a reactive scope
   that disposes them at scope end; `effect(...)` also returns a disposer handle for manual early
   disposal. (Open sub-question for S-effects: whether the top-level implicit scope is the isolate
   lifetime — resolve when we reach that slice.)

## Architecture — where the pieces live

Mirror the channels/isolates split exactly (the pattern that keeps them oracle-safe):

- **`noeta-value`:** lightweight id handles only. A `NodeId` (like `ChannelId`/`ScopeId`) plus new
  `Payload::Signal(NodeId)` / `Payload::Computed(NodeId)` / `Payload::Effect(NodeId)` (or a single
  `Payload::Reactive(NodeKind, NodeId)`). The `Value` carries just the id; no graph state in the value.
- **Shared reactive-graph core (a `ReactiveGraph` struct, backend-agnostic):** the deterministic
  bookkeeping — the node table, dependency edges (bidirectional: sources↔subscribers), dirty flags, the
  **current-subscriber stack** (the dynamic scope that turns a `signal.get()` inside a computed/effect
  body into a subscription), the dirty-effect queue, and the topological flush. This is pure data + graph
  algorithms — no I/O, no closure calls — so it is trivially deterministic and unit-testable in isolation.
- **The backend callback seam (the one backend-specific step):** "recompute node N" must run node N's
  stored closure, and closure invocation is backend-specific. Each backend drives it through its existing
  re-entry seam — eval's `call_closure`, the VM's `call_value` — exactly as `Future`/async and isolate
  spawn already re-enter user code from runtime state. So the graph says *which* node to run and *when*;
  the backend runs it. The dependency-capture handshake (push node onto the current-subscriber stack →
  invoke closure → pop) brackets that call in the shared core.

The result: the deterministic, oracle-critical half is shared and backend-agnostic; the only per-backend
code is the one-line "invoke this closure" bridge, which both backends already have and already exercise.

## Surface API (stdlib)

```noeta
count = signal(0)
doubled = computed(fn() => count.get() * 2)     // lazy; recomputes when count changes
effect(fn() => echo "count is ${count.get()}")  // eager; reruns when count changes

count.set(5)      // marks doubled dirty, enqueues the effect; flush runs the effect once
count.update(fn(n) => n + 1)                     // read-modify-write convenience

echo doubled.get()   // 12 — recomputed lazily on read
```

- `signal(initial: T): Signal<T>` — `.get(): T`, `.set(v: T)`, `.update(fn(T) => T)`.
- `computed(fn() => T): Computed<T>` — `.get(): T` (lazy, memoized). Read-only.
- `effect(fn() => void): Effect` — runs immediately, reruns on dependency change; `.dispose()`.
- Reads inside a computed/effect body auto-subscribe; reads outside any reactive context are plain reads
  (no subscription). `.set()` outside a flush starts one; `.set()` inside an effect coalesces.

Type surface: `Signal<T>` / `Computed<T>` / `Effect` are stdlib generic types (erased, like the rest);
the checker types `.get()`/`.set()` against `T`. Whether these ride existing generic-class machinery or
need a small native-type registration is an S1 detail.

## Slices (proposal)

- **S0 — graph core + unit tests (no language surface). DONE.** New leaf crate **`noeta-reactive`**: a
  value-generic `ReactiveGraph<V>` (node arena + free-list, bidirectional dependency edges, the
  current-computing stack for auto-subscription, lazy-memo `computed`, eager queued `effect`, a
  deterministic batched flush) driven by a backend-supplied `run: &mut dyn FnMut(V) -> V` closure seam.
  Reentrancy (a recompute runs a closure that reenters to read its deps) is handled by `RefCell` interior
  mutability with the discipline that **no borrow is held across `run`**. 9 property tests prove:
  signal round-trip, effect run-once/rerun, computed laziness+memoization, **diamond sink recomputes once
  glitch-free**, deterministic effect order by `NodeId`, dispose-unsubscribes-and-frees-no-leak, untracked
  peek, dynamic-dependency resubscription, and set-inside-effect coalescing into the flush. `unsafe`-free,
  clippy/fmt/**miri** all clean. The determinism + glitch-freedom + no-leak proofs live here, before any
  backend or syntax exists.
- **S1 — `signal` in both backends. DONE.** `signal(v)` (a `Builtin`, added to `PRELUDE_NAMES` + both
  backends' `Builtin` enums) + `.get()`/`.set(v)`, wired through the shared `ReactiveGraph` on each
  backend (`Rc<ReactiveGraph<…>>` so a flush can borrow the graph and `self` independently). Observable
  closure-free via get-after-set, so it stands alone without S2. Value repr: `Payload::Reactive(NodeKind,
  NodeId)` (VM) / `Value::Reactive(…)` (eval), a GC leaf carrying `noeta_reactive::{NodeId, NodeKind}`
  (shared, not duplicated). **Refcount discipline:** the VM stores a RAII `GcVal` (Clone=retain,
  Drop=release) so the graph's internal clones/drops keep the manual refcount exact; eval's `Rc`-Value is
  correct for free. Checker: reserved `Signal<T>` type + `prelude_return` + `signal_method`/params, so
  `.set` type-checks (**E0007** on mismatch) and `.get()` recovers `T`. **The leak oracle earned its
  keep:** the trace cycle-collector reclaimed a signal's content (held only by the graph, invisible to
  the from-roots sweep) and `clear()` then double-freed it — fixed by feeding the graph's held values in
  as **GC roots** (`for_each_value`), the reactive analogue of scanning channel buffers. 3 conformance
  cases (get/set, independent cells, negative type-mismatch). Conformance 445, differential 434/0-skipped,
  jit-differential 434, leak 0 both backends, workspace green, clippy/fmt clean. `.update` folds in with
  S2's closure-driving machinery.
- **S2 — `effect`. DONE.** `effect(fn)` (a `Builtin`) runs its body immediately and reruns it when a
  signal it read changes; `.dispose()` unsubscribes it; `signal.set`/`.update` now drive a flush. The
  crux — **`drive_flush`** on each backend: clone the `Rc<ReactiveGraph>` out, then `graph.flush(run)`
  where `run` invokes each effect body through the backend's call seam (`call_value`/`call`), with the
  refcount discipline (VM: peek the graph-cloned `GcVal` body via `.get()` and let it drop, since
  `call_value` only *borrows* the callee) and **deterministic abort capture** (first body to abort by
  flush order stops the rest and propagates, identically on both backends). `.update(fn)` is the first
  `.set` that runs a closure (read → call → set → flush). Dependency capture is automatic and *dynamic*:
  a signal read inside a running effect subscribes it, and each rerun resubscribes — so a `cond ? a : b`
  effect tracks exactly the branch it took. 4 conformance cases (reacts, dispose, **dynamic
  resubscription**, deterministic fan-out order). Leak oracle 0 both backends (effect bodies are
  refcount-exact through the call boundary; dispose/clear free them). Conformance 449, differential
  438/0-skipped, jit-differential 438, workspace green, clippy/fmt clean.
- **S3 — `computed`. DONE.** `computed(fn)` (a `Builtin`) — a lazy, memoized derivation with `.get()`
  (read-only). This is where `.get()` runs a *real* body for the first time: the new **`read_reactive`**
  on each backend clones the `Rc<ReactiveGraph>` out and drives `graph.read` with a `run` callback that
  invokes a dirty computed's body through the backend's call seam (`call`/`call_value`) — the read twin
  of S2's `drive_flush`, with the same VM refcount discipline (peek the graph-cloned `GcVal` via
  `.get()`) and deterministic abort capture (the first recompute to abort stops the rest, transitively).
  The graph core (S0) already had `computed` fully — laziness, memoization, transitive computed-reading-
  computed, and glitch-free diamonds are all its property tests — so S3 is pure wiring: no scheduling
  logic moved into the backends. A `computed` is created dirty and computes on first read (no flush at
  creation, unlike `effect`); reads memoize and clear dirty; a `set` upstream dirties it (lazily,
  transitively) so the next read pulls fresh exactly once. **Kind-guarded dispatch:** the reactive
  method block on both backends now matches `(kind, method)` — `get` on signal/computed, `set`/`update`
  on signal, `dispose` on effect — so `computed.set()` (or any invalid pair) falls through to the same
  **E0005 "no method … on computed"** any built-in type gives, identically on both backends, instead of
  reaching the core's signal-only `set`. Checker: reserved `Computed<T>` type + `computed_method`(get→T)
  + `prelude_return` (`computed(fn() -> T)` carries the closure's return type as its arg so `.get()`
  recovers `T`) + `method_params`. 5 conformance cases (lazy+memo witnessed by a "computing" echo,
  transitive pull order, glitch-free diamond feeding an effect, an effect witnessing a computed, and the
  read-only negative). Conformance 454, differential 443/0-skipped, jit-differential 443, leak 0 both
  backends, workspace green, clippy/fmt clean.
- **S4 — disposal/ownership + GC interaction.** Scope ownership, cycle-collector interaction (the
  signal↔subscriber graph is cycle-shaped by design), miri clean. Confirm the leak oracle stays 0 across
  create/dispose churn.
- **S5 — hardening + docs.** Negative cases, the reactivity wiki page, a bench (large fan-out flush), and
  the deferred-registry entry for the transport half.

(Slice boundaries firm up after S0 proves the core; S1/S2 likely land together since a signal with no
consumer isn't observable.)

## Oracle posture (the spine — non-negotiable)

- **Determinism is the whole game.** Effect/flush order must be a pure function of the program (topological
  + insertion-order tie-break), with zero wall-clock/hash-order/thread dependence — then `--differential`
  (eval vs VM byte-identical `RunResult`) covers the feature by construction, `0 skipped`.
- **Leak oracle is the disposal proof.** The reactive graph is deliberately cyclic (signal↔subscriber); a
  missed unsubscribe on dispose shows up as non-zero residency immediately. Every slice runs it.
- **miri** over the new graph mutation (RefCell/id-table churn) stays clean.
- Both backends share one `ReactiveGraph` core, so a behavior difference is structurally hard to introduce
  — the only per-backend code is the closure-invocation bridge, which is already oracle-covered by async.

## Deferred (explicitly out of this milestone — the transport/consumer layers)

Tracked here, land when their prerequisites exist:

- **WS minimal-diff push / LiveView** (§9.4, §9.5) — needs the bundled HTTP/WS server. The reactive core
  exposes "which computeds changed this flush" as the hook the diff layer will consume.
- **Reactive persistence** (§9.12) — DB-change→signal invalidation. R&D; needs the DB layer (§11.4) and
  the open consistency-model questions resolved.
- **HMR** (§9.14) — code-change→UI through the graph. Needs the persistent runtime + server.
- **Synced/CRDT `synced_signal`** (§9.15) — needs p2p stack; opt-in per app.

## Verification (every slice)

- `noeta test` conformance green; `noeta test --differential` matched / **0 skipped** / backends agree.
- Leak oracle residency 0 across signal/effect/computed create+dispose churn.
- `cargo test --workspace`, `clippy --all-targets`, `fmt --check` clean; miri clean over the graph core.
- The slice's **benchmark** where a hot path is touched (S5 fan-out flush), before/after recorded.
- Standard commit trailers.
