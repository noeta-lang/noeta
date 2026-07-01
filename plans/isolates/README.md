# Isolates & channels — inter-isolate parallelism (the CPU-bound layer)

**Status: BUILDING — I.0 + I.1 + I.2 + I.3 + I.4 (a/b/c) DONE, I.5 (finalize) next.** This is the parallelism half of architecture §7 (the
async half — intra-isolate `async`/`await` + structured concurrency — is complete: see
`plans/coroutines/track-a-async.md`). It is a **milestone**, not a slice, and the successor track the
object-model redesign explicitly deferred `!Send` enforcement to ("the concurrency milestone … which
is where the boundary to reject a `!Send` class will live", object-model README).

Provenance: the user's 2026-07-01 request to build the async follow-ons "1–5 + 7"; items 1–5 (A.6–A.10)
shipped as in-oracle slices; item 7 (this) is the one that needs its own design pass first.

## What it is (and isn't)

- **Is:** true multi-core parallelism via **isolates** — shared-nothing units of execution, each with
  its own heap, communicating only by **message-passing over typed channels**. This is the escalation
  for CPU-bound work (§7: "isolates are for parallelism (CPU), async is the everyday tool (I/O)").
- **Isn't:** shared-memory threading. Userland never sees threads, locks, or `Arc<Mutex>` (§7 keeps
  those out of the language). It isn't async — async is cooperative within one isolate; isolates are
  parallel across heaps. The two **compose**: an async isolate offloads a CPU job to a worker isolate
  over a channel and `await`s the result.

## The deciding constraint (same one that shaped every prior track): the differential oracle

Real inter-isolate parallelism is nondeterministic (OS threads, real scheduling) — it cannot run in
the two-backend differential. So this milestone extends the **"simulate deterministically, deploy
real"** split the codebase already applies twice:

| Seam | Deterministic (in-oracle) | Real (CLI-only, out-of-oracle) |
|---|---|---|
| Host IO | `SandboxHost` (VFS, logical clock) | `RealHost` (disk, tokio) |
| Async scheduler | `SandboxExecutor` (logical time) | `RealExecutor` (wall-clock, tokio) |
| **Isolates (this)** | **`SandboxScheduler`** — cooperative, single-thread, deterministic isolate interleaving + FIFO channels | **`RealScheduler`** — OS threads, one runtime per isolate, real channels |

So a program using isolates **type-checks and runs identically in the sandbox** (deterministic
interleaving, differential-covered), and runs with real parallelism on the CLI. The scheduling logic
is a trait (`IsolateHost`/`Scheduler`) held as `Box<dyn …>`, exactly like `Host`/`Executor`; both
backends drive the *same* deterministic sandbox scheduler so they agree by construction, and the
sandbox never spawns a thread.

**Why the sandbox is even possible:** isolates are shared-nothing and communicate only by *copied*
messages, so a single-threaded cooperative simulation (run isolate A until it blocks on a channel,
switch to B, …) is observationally faithful to real threads for any well-typed program — there is no
shared mutable state whose interleaving could differ. Determinism comes free from the shared-nothing
model, the same way it did for generators (pull-based) and the async sandbox (logical clock).

## Surface (settled with the user, 2026-07-01)

Two primitives:

1. **`Channel<T>` — bounded, both ends async (backpressure from the start).**
   - `channel::<T>(capacity)` → `(Sender<T>, Receiver<T>)` — the split-endpoint pair (Rust/Go-typed
     directions). Direction is unrepresentable-when-wrong (you can't `recv` from a `Sender`); "closed"
     means concretely *all senders dropped*, so `recv` can terminate.
   - `tx.send(v): Future<void>` — **async**; suspends when the buffer is full (backpressure).
   - `rx.recv(): Future<?T>` — **async**; suspends when empty, `none` once closed **and** drained.
   - The queue is **scheduler-owned**, never shared memory; endpoints are just ids (trivially `Send`).
     Channel messages are **copied** into the queue (bounded by `capacity`, so memory is bounded).
2. **`isolate f(args)` — a prefix keyword paralleling `spawn f(args)`.** Uniform with `spawn`: both are
   prefix keywords on a call, both yield a `Future<T>` handle, both live in `concurrent { }`, both feed
   `all`/`race`. The *only* differences: `isolate` runs the body in a **fresh isolate** (own heap, real
   parallelism on the real executor) and so constrains `args`/result to `Send`. `spawn` stays general
   (any future expr); `isolate` is restricted to a **direct call** `isolate f(args)` so it knows what
   to ship. Orphan `isolate` (no owning scope) is **E0041**, same rule as `spawn`.

Isolate panic → **re-panics the parent at `.await`** (consistent with tasks; recoverable failures use
`Result` returns as everywhere else).

## The `Send` boundary + cross-thread sharing (the type-system + memory core)

Only **`Send` values may cross an isolate boundary**. This is where the object model's deferred axis
finally bites, and — decided with the user — **the value/reference axis IS the shareability axis**:

- **Value types** (`struct`, primitives, `bytes`, tuples, enums, `List`/`Map`/`Set`/`Option`/`Result`
  of `Send`) **are `Send`**. A container is `Send` iff its elements are; a `struct`/`enum` iff all its
  fields/payloads are (checked structurally against the type registry, with a visited-set for recursive
  types).
- **Reference types** (`class` — identity, shared mutation) **are `!Send`**: sending one would share a
  mutable heap object across isolates (breaks shared-nothing + non-atomic refcounts) or silently copy
  away its identity. A `class` value at a boundary is **E0042** (the new code — closes the object-model
  arc's parked `!Send` deferral). Want to send a class's data? Convert it to a `struct` — explicit.
- **`dyn`** is conservatively `!Send` for v1 (can't prove a `dyn` isn't a class); relaxable later.

### Cross-thread sharing by *borrow*, not atomic refcounts (settled — built in this milestone)

Immutable value types are race-free to *read*, but their refcount is mutable metadata — so naive
cross-thread sharing would race the count, normally forcing atomic refcounts (the tax §7 avoids). We
sidestep that entirely by leaning on structured concurrency's guarantee: **`concurrent { }` joins every
isolate at `}`, so anything the scope shares outlives all of them.** That lifetime bound makes
**borrowing without refcounting** safe across threads (the Rust scoped-threads / `rayon::scope` trick):

- The scope owns a **shared-immutable region**; a value graph shared into it lives for the scope's life.
- Cross-isolate references are **`shared`-tagged Values**, and `retain`/`release` on a shared-tagged
  pointer are **no-ops** — nothing is ever written (not the data, not the count), so nothing races.
  Lock-free by construction; no atomic ops.
- At the scope join (a thread-join barrier → happens-before all isolates finished), the region is freed
  wholesale.

**Where each mechanism applies:** `isolate f(bigdata)` **borrow-shares** its argument graph across all
isolates in the scope — **one** promotion-copy into the region, then zero-copy for N workers (the
"share across threads" win). **Channel messages** are **copied** per message (they're streamed, not
scope-lifetime-bounded; `capacity` bounds the memory).

**Cost:** Values gain a local-vs-`shared` distinction and `retain`/`release` branch on it (miri-covered).
Single-isolate programs never allocate in the shared region, so the branch always takes the local path —
the whole conformance/differential corpus is unaffected. A later *move-promote* could make even the
first copy zero-copy; one copy is the honest v1.

**Oracle:** the sandbox is single-threaded and just **copies** (deterministic, in-oracle); the real
executor **borrow-shares** across threads. For immutable value types these are observationally identical
(value types have no identity and no mutation — `===`/mutation are class-only, and class is `!Send`), so
the differential holds and sharing is a real-executor perf property (out-of-oracle), with the heap
machinery exercised by the real path + miri.

## How it composes with async (Track A)

- `rx.recv()` returns a `Future` → it plugs straight into the async scheduler (`await` it; a blocked
  `recv` suspends the isolate's current task, and in the sandbox yields to the next runnable isolate).
- An isolate handle is a `Future<T>` (Track A.3b's handle shape) → `all`/`race` (A.9) work over
  isolate handles unchanged.
- `concurrent { }`'s structured-scope discipline generalizes to isolate scopes (join at `}`).

So most of the *consumer* surface (await, handles, all/race, structured join) is already built; this
milestone adds the *producer* side (spawn an isolate, cross the Send boundary, channels) and the
deterministic multi-isolate scheduler underneath.

## Sub-slices (settled)

- **I.0 — `isolate` surface + the `Send` classifier + E0042; executable in-oracle as a task. ✅ DONE
  (`baa72ab`).** `isolate` prefix keyword (a flag on `Expr::Spawn`), structural `Send`/`!Send` classifier
  (`Checker::is_send`, visited-set for recursion; struct/enum = `Send` iff fields/payloads are, class =
  `!Send`, `Future`/`Iterator`/`FileHandle`/closures/`dyn` = `!Send`, inference hole permissive), E0042 on
  a non-`Send` arg/result of an `isolate` call (or `isolate` on a non-call), orphan-`isolate` → E0041. In
  the **sandbox** an isolate is observationally a task (single-thread; `Send` value args copy-invisible),
  so `isolate` lowers to the existing `Rvalue::Spawn` and is fully executable + differential-covered — no
  gate. Real heap separation is I.4. Closed the object-model `!Send` deferral (E0042 ships here).
  Conformance 376 (isolate/isolate_not_send/isolate_orphan). Next free diag **E0043**.
- **I.1 — bounded `Channel<T>` + async `send`/`recv`/`close`. ✅ DONE.** `channel::<T>(capacity)` is a
  turbofish keyword (an `Expr::Channel` → `Rvalue::MakeChannel`/`Op::MakeChannel`, message type
  checker-only, capacity the only runtime operand) yielding a `(Sender<T>, Receiver<T>)` tuple of
  scheduler-owned endpoint ids (`Value::Sender`/`Receiver`). `tx.send(v): Future<void>` (async — leaf
  `Value::ChannelSend` future, enqueues on a poll when the buffer has room, else suspends → backpressure),
  `rx.recv(): Future<?T>` (async — leaf `Value::ChannelRecv`, dequeues → `some(v)`, `none` once closed and
  drained, else suspends), `tx.close(): void` (synchronous). Messages pass **by copy** (the queue owns its
  own reference); deterministic FIFO. The channel table lives in each backend (`channels: Vec<Channel>`,
  mirrored, like `scopes`) — no `SandboxScheduler` *trait* yet: with a trivial VecDeque queue the seam
  buys nothing until `RealScheduler` (I.4), so the trait extraction is deferred there (consistent with how
  the codebase extracts a trait at the second impl). A `channel_progress` counter distinguishes a
  channel op that unblocks a sibling from a stalled round (a `send`/`recv`/`close` is progress even when no
  task *completes*), so the deadlock detector doesn't misfire on a producer/consumer pair. Endpoint
  `Send`-ness propagates its message type (`Sender<T>`/`Receiver<T>` is `Send` iff `T` is), so a
  `Receiver<class>` is `!Send` → E0042 at an isolate boundary. In-oracle: differential 100% / leak 0 /
  miri-clean. Conformance 378 (`channel`, `channel_endpoint_not_send`). **Deferred:** capacity-0
  rendezvous (a 0-cap channel deadlocks — send never finds room); auto-close on all-senders-dropped
  (needs endpoint drop-tracking — `close()` is explicit for now); a `SandboxScheduler` trait (I.4).
- **I.2 — deterministic multi-isolate interleaving polish. ✅ DONE.** Verified N isolates + N channels
  interleave deterministically in the sandbox across the block-points (`send`-full, `recv`-empty,
  `.await`) with structured join — and proved **no scheduler code change was needed**: the A.7 cross-scope
  round-robin (`poll_all_scopes_round` walks every open scope in order, so siblings at all levels advance)
  composed with I.1's channel block-points + progress-aware deadlock detection already delivers it. A
  verification slice (5 conformance tests, no source change): `channel_pipeline` (3-stage `source →
  square → sink` over two capacity-1 channels — backpressure both directions, close propagates down the
  chain), `channel_fanin` (two producers share one `Sender`, a cross-scope consumer drains, inner scope
  joins producers before an explicit close), `channel_interleave` (a capacity-1 channel forces strict
  producer/consumer alternation — pins the exact **observable side-effect order**, the sharpest cross-
  backend determinism test), `isolate_channel` (three worker **isolates** report squares over a channel,
  structured join, `Send`-checked endpoints), `channel_deadlock` (a never-fed/never-closed `recv`
  deadlocks *deterministically* — E0010, exit 1, no hang, both backends). Differential 374 / leak 0 both
  / conformance passes. (Much of the scheduling reused Track A's cooperative scheduler, as predicted.)
- **I.3 — shared-immutable region + `shared`-tag heap change. ✅ DONE.** The borrow-not-refcount
  machinery, entirely in `lang-value`, `miri`-proven, and (by design) called by neither backend yet —
  the sandbox keeps copying per isolate (in-oracle), so the whole conformance/differential/leak corpus
  is byte-identical; I.4 wires it to real threads. Decided with the user: a **runtime header bit**, not
  a `@shared` directive (shared-ness is dynamic and per-value-per-scope — the same type has local and
  shared instances at once — and it's an observationally-invisible perf optimization, so it must not be
  a declaration-site annotation; a directive would only name the boundary root and would compile *down
  to* the tag anyway, since retain/release fire at arbitrary interior alias sites where only an
  object-carried flag can answer "is this shared?"). Pieces: (1) a `shared: bool` on `ObjHeader`, set
  **once at promotion** before publication and never rewritten (so concurrent non-atomic *reads* are
  race-free); `inc_ref`/`dec_ref`/`release` **no-op** on a shared object — its count is never written,
  so no atomics and no cross-thread count race. Shared objects live **outside** the refcount *and* the
  cycle collector (a dedicated `alloc_shared` skips the GC registry; never buffered as a cycle root —
  shared graphs are acyclic since cycles need identity+mutation, which are `class`-only, and `class` is
  `!Send`). (2) `SharedRegion { objects: Vec<Value> }` — the explicit struct the scope owns (chosen over
  a thread-local for borrow-checked lifetime + clean miri isolation): `promote(root)` deep-copies a
  value graph into fresh `shared`-tagged objects (memoized by NaN-box word, so a DAG is copied **once**
  and sharing structure is preserved; immediates pass through; a `!Send` payload is `unreachable!` — the
  E0042 classifier guarantees it can't arrive), the original left independent (a copy, not a move);
  `free_all(self)` reclaims the whole graph wholesale at the join (shallow per-object free, children are
  separate entries) — leak-balanced (+N at promote, −N at free_all). 4 miri tests (no-op rc storm,
  deep-copy independence after freeing the original, DAG dedup, immediate pass-through); full lang-value
  miri suite 43/43. Public API: `SharedRegion`, `Value::is_shared`. Conformance/differential/leak
  unchanged (374/100%/0-both). **Deferred to I.4:** move-promote (make even the first copy zero-copy);
  a range-check region-arena alternative to the per-object bit; region interaction with a mid-scope
  backup GC pass (shared objects are already outside the registry, so this is a non-issue in practice).
- **I.4 — `RealScheduler` (OS threads), CLI-only / out-of-oracle.** Real parallelism + real borrow-
  sharing across threads; integration-tested like `RealHost`/`RealExecutor` (incl. a big-input-no-copy
  check). **Split into I.4a (backend routing) + I.4b (real threading)** — the eval tree-walker's `Value`
  is `Rc`-based (`!Send`), so it can never carry cross-thread parallelism; only the VM (NaN-boxed heap,
  thread-local accounting, the home of I.3's `SharedRegion`) can. Decided with the user: **switch the
  CLI's real-run path to the VM entirely** (eval stays the differential reference + sandbox).
  - **I.4a — route real execution through the VM. ✅ DONE.** `lang run`/`@test`/`bench` now compile to a
    bytecode `Module` (`lang_compiler::compile_with_sites` off the already-`checked` program — the same
    Core-IR lowering + drop/reuse passes the eval path open-coded, then IR → bytecode) and run it on
    `VmBackend::run_module_with_host{,_and_executor}` with `RealHost` + `RealExecutor`. No eval fallback:
    every program that parses+checks compiles (the differential holds VM coverage at 100% *by
    construction*), so a compile `Err` is surfaced as an internal error, not a silent downgrade to a
    second backend. The REPL keeps eval (stateful `Session`; isolates typed there stay cooperative — a
    documented limitation). Verified behavior-neutral: a full old-CLI(eval) vs new-CLI(VM) sweep over
    387 corpus programs found **zero backend-logic divergences** — the only differences were real-clock
    timing nondeterminism in sleep-interleaving programs (nondeterministic on *both* backends, inherent
    to the out-of-oracle real executor) and the `args` binary-path harness artifact. CLI tests 45+8,
    conformance/differential/leak unchanged (374/100%/0 both), clippy+fmt clean. Also dropped the CLI's
    now-dead `lang-ir`/`lang-ir-passes` deps (it goes through `lang-compiler` now).
  - **I.4b — real OS-thread isolates via copy-at-the-boundary. ✅ DONE (fork-join).** A channel-free
    `isolate f(args)` runs on its own OS thread with true multi-core parallelism (verified: two 300ms
    isolates finish in ~365ms, not ~600ms). Built as four steps: (1) `5d53605` `SpawnIsolate`
    infrastructure (inert, `real_isolates` compile flag — sandbox lowers `isolate`→`Spawn`, byte-
    identical); (2) `c0eadf4` the `IsolateFuture` leaf value; (3) `8e3c482` the `Wire` marshalling
    (`marshal`/`rebuild`, shapes by `Module.shapes` index, round-trip + miri tested); (4) `1e65d6e` the
    handler + CLI flip. **Decided with the user (mid-build, after finding the cross-thread-channel ×
    cooperative-scheduler deadlock hazard): land fork-join now, defer channels to I.4c.** The handler:
    real thread when the VM is parallel and no arg ships a channel endpoint, else a cooperative task
    (`call_value` builds the future + register — identical to `spawn`, so `@test`/`bench` and
    channel-shipping isolates never regress); the worker builds its own VM (`Vm::load`, no `main`
    side-effects) with its own heap/host/executor from an injected `IsolateFactory`, seeds globals from
    the parent's marshalled snapshot, runs the callee to completion, marshals the result back over an
    mpsc; the parent registers an `IsolateFuture` task harvested by `try_recv`, with `inflight_isolates`
    keeping a pending isolate from false-deadlocking. Soundness: no `Value`/`Rc` crosses a thread — only
    `Arc<Module>` (Send+Sync) is shared; args/results deep-copied via `Wire`. Sandbox/differential
    untouched (374/100%/agree, leak 0). **v1 limitations (→ later):** a worker snapshots only
    marshallable globals (functions + value-type constants; a class-instance global is skipped, so an
    isolate referencing one fails at use); isolate teardown skips cycle collection (a shared-nothing
    isolate forming a global cycle is out of scope). Original I.4b design (copy-not-borrow rationale):
    runtime `Value`s carry a non-atomic `Rc<Shape>` (`Value::shape()` clones it), so *borrow*-sharing
    structs/enums across threads is unsound until shapes are thread-safe — I.3's `shared` tag covers the
    object refcount but not the inner `Rc<Shape>`. Zero-copy borrow-share is a later slice (needs
    `Rc<Shape>→Arc<Shape>` + the big-input-no-copy check).
  - **I.4c — cross-thread channels. ✅ DONE (`11825cd`).** Shipping a `Sender`/`Receiver` into a real
    isolate shares one queue across threads, so a producer isolate + consumer (or two isolates) pass
    messages across cores. **The deadlock hazard is resolved by keeping the cooperative poll model:** a
    shared channel is a `Mutex`-guarded queue polled *non-blockingly* (Pending on full/empty), never a
    thread block — each thread's scheduler makes progress by re-polling, so a send that can't proceed
    suspends that isolate's task rather than parking the thread (which would stall its siblings / the
    parent). Pieces: `Channel` becomes `Local` (cooperative in-VM `VecDeque<Value>`, sandbox/non-parallel,
    unchanged) vs `Shared(Arc<ChannelCore>)` (cross-thread, parallel VM); in a parallel VM every channel
    is `Shared` from birth. `ChannelCore` (isolate.rs) = `Mutex<VecDeque<Wire>>` + closed, with
    `send_state`/`try_send`/`try_recv`/`close`/`is_open`. Shared send marshals to `Wire` only when there's
    room (cheap full poll); shared recv rebuilds into the local heap. `Wire` gains `Sender`/`Receiver`
    (clone the `Arc`); `marshal` ships an endpoint over a shared channel, rejects a `Local` one; `rebuild`
    registers the shared core into the worker's table; the I.4b channel-endpoint exclusion is removed. The
    stall-yield guard generalizes: keep polling while any *open shared channel* could still be fed/drained
    by another isolate thread (so a cross-thread producer/consumer never false-deadlocks). Teardown drops
    `Shared` channels (`Wire` holds no heap `Value`s). Soundness unchanged (only `Arc`-shared immutable
    state crosses; messages deep-copied). Sandbox/differential byte-identical (all `Local` in-oracle):
    374/100%/agree, leak 0. Tests: 4 marshalling unit (incl. shared-endpoint ship/rebuild + `Local`
    rejection), miri-clean; 2 CLI integration (producer-isolate→parent-consumer = 10; capacity-1
    backpressure = 3, proving no cross-thread deadlock). **v1 deferred:** capacity-0 rendezvous still
    deadlocks (as I.1); auto-close on all-senders-dropped (explicit `close()` for now).
  - **I.4b (original bundled description, retained for reference):**
    Runtime `Value`s carry a non-atomic `Rc<Shape>` (every Object/Enum; `Value::shape()` does
    `shape.clone()`), so *borrow-sharing structs/enums across real threads is unsound* until shapes are
    made thread-safe — and I.3's `shared` tag covers only the object's own refcount, not the inner
    `Rc<Shape>`. So I.4b crosses the thread boundary by **faithful copy**, not borrow: no `Value` and no
    `Rc` ever crosses a thread; the only shared thing is `Arc<Module>` (the compiled `Module` is
    `Send + Sync` — fully index-based, no `Rc`), which is safe. Zero-copy borrow-share is deferred to
    **I.4c** (where the `Rc<Shape>→Arc<Shape>` thread-safety change, the `unsafe Send`, `SharedRegion`
    wiring, and the big-input-no-copy check live). "Copy is the honest v1" — exactly the plan's framing.
    Concrete pieces (one slice, full Approach A incl. cross-thread channels):
    - **Lowering rework (enabler).** `isolate f(args)` pre-builds its future (`f(args)` captures args in
      the *parent* heap) and lowers to `Rvalue::Spawn`, dropping `is_isolate`. A pre-built future can't
      move to a thread, so split it: a dedicated `Rvalue::SpawnIsolate { callee, args, span }` +
      `Op::SpawnIsolate` carrying the **unbuilt** callee + arg atoms (isolate is already the restricted
      direct-call form). Sandbox interprets it exactly as `Spawn` (build the future, cooperative task) →
      differential byte-identical, never threads. `spawn e` keeps lowering to `Rvalue::Spawn`.
    - **Wire form** (`lang-vm`, `Send`): a faithful `enum Wire` (primitives, str/bytes, list/tuple/set/
      map, `Struct`/`Enum` keyed by shape *name* [+ variant], channel endpoints as `Arc<ChannelCore>`).
      `marshal(Value)->Wire` on the source thread; `rebuild(Wire)->Value` on the dest thread (allocs in
      the dest heap using the dest's own `Rc<Shape>` looked up by name). A `!Send` payload is
      `unreachable!` (E0042 guarantees it can't reach a boundary).
    - **Cross-thread channels:** a `channel::<T>(cap)` entry becomes a shared `Arc<ChannelCore>` (bounded
      `Mutex<VecDeque<Wire>>` + `Condvar` + close flag) in the real path; endpoints stay `Value::Sender/
      Receiver(index)` per-VM but the table entry holds the `Arc`, so shipping an endpoint into an
      isolate marshals to `Wire::Sender(Arc-clone)` and the worker registers it locally. `send`/`recv`
      block the (real) worker thread on full/empty — no cooperative poll needed cross-isolate.
    - **Thread lifecycle:** `Op::SpawnIsolate` (real) marshals args, clones `Arc<Module>`, spawns a
      `std::thread` that builds its own `RealHost`+`RealExecutor`+`Vm`, rebuilds args, runs the callee,
      marshals the result back over a oneshot. The handle is a leaf future polled by `try_recv`
      (Ready→rebuild result / Pending); the scheduler tracks `inflight_isolates` so a pending isolate is
      *progress* (a thread is working), not a false deadlock; `concurrent {}` joins the threads at `}`.
    - **Seam:** a `parallel_isolates` flag set by the real entry point (`run_module_with_host_and_
      executor`); the sandbox path leaves it false (cooperative, unchanged). Integration-tested
      out-of-oracle (real threads actually run in parallel; results/messages round-trip).
- **I.5 — finalize:** docs (§7 alignment), deferred rows (durable queues, worker pools, app-lifetime
  `TaskScope` via DI — §7.2 framework patterns, *not* language constructs), mark complete.

Starting point: **I.0** — pure front-end + in-oracle execution-as-task, closing the object-model
deferral, independent of the channel/scheduler/real-thread work that follows.
