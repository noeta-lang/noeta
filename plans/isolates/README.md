# Isolates & channels — inter-isolate parallelism (the CPU-bound layer)

**Status: BUILDING — I.0 + I.1 + I.2 DONE, I.3 next.** This is the parallelism half of architecture §7 (the
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
- **I.3 — shared-immutable region + `shared`-tag heap change.** Borrow-share (no-op rc on shared
  pointers), miri-covered. Sandbox still copies (in-oracle); the machinery lands here for I.4 to use.
- **I.4 — `RealScheduler` (OS threads), CLI-only / out-of-oracle.** Real parallelism + real borrow-
  sharing across threads; integration-tested like `RealHost`/`RealExecutor` (incl. a big-input-no-copy
  check).
- **I.5 — finalize:** docs (§7 alignment), deferred rows (durable queues, worker pools, app-lifetime
  `TaskScope` via DI — §7.2 framework patterns, *not* language constructs), mark complete.

Starting point: **I.0** — pure front-end + in-oracle execution-as-task, closing the object-model
deferral, independent of the channel/scheduler/real-thread work that follows.
