# Isolates & channels — inter-isolate parallelism (the CPU-bound layer)

**Status: DESIGN — for sign-off. No code yet.** This is the parallelism half of architecture §7 (the
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

## Surface (proposed — the main thing to sign off)

Two primitives, both **library-shaped over a runtime seam** (like `spawn`/`sleep`), not new syntax
where avoidable:

1. **`Channel<T>`** — a typed, bounded (or unbounded, TBD) message queue.
   - `channel::<T>()` → `(Sender<T>, Receiver<T>)` (or a single `Channel<T>` with `.send`/`.recv`).
   - `tx.send(v)` — hand a value to the channel (moves/copies it across the boundary).
   - `rx.recv(): Future<?T>` — **async** receive; `none` when the channel is closed and drained. Ties
     into the async layer (you `await` a `recv` inside an async isolate).
2. **Isolate spawn** — `isolate(fn, args)` (name TBD) runs `fn(args)` in a **fresh isolate** with its
   own heap, returning a handle to await its result (itself a `Future<T>`, reusing Track A's handle
   shape) *and/or* wired to channels. Structured by default: an isolate scope joins its children
   (mirroring `concurrent { }`), so no orphaned isolates — the same "nothing dangles" rule §7.2 wants.

The exact spelling (free fns vs a `Channel`/`Isolate` type, bounded vs unbounded, `send` blocking vs
async) is what this design pass exists to settle **with the user** before any code.

## The `Send` boundary (the type-system core of this milestone)

Only **`Send` values may cross an isolate boundary** (as a channel message or an isolate argument/
result). This is where the object model's deferred axis finally bites:

- **Value types** (`struct`, primitives, `bytes`, tuples, enums, `List`/`Map`/`Set` of `Send`) **are
  `Send`** — they are copied across the boundary (deep copy / serialize), so each isolate owns its own
  heap graph and non-atomic refcounts stay sound.
- **Reference types** (`class` — identity, shared mutation) **are `!Send`**: sending one would either
  share a heap object across isolates (breaks shared-nothing + non-atomic refcounts) or silently copy
  away its identity. So a `class` value at a boundary is a **compile error** — the new diagnostic this
  milestone adds (**E0042**, the next free code). This is the check the object-model arc parked here.
- **Transfer mechanism:** reuse the existing deep marshalling. `to_bytes`/`from_bytes` (P-PACK) and the
  `NativeValue` deep tree already serialize value graphs; a channel message is "deep-copy the value
  into the receiver's heap." No new serialization format — the boundary is the existing marshalling
  seam applied isolate-to-isolate.

## How it composes with async (Track A)

- `rx.recv()` returns a `Future` → it plugs straight into the async scheduler (`await` it; a blocked
  `recv` suspends the isolate's current task, and in the sandbox yields to the next runnable isolate).
- An isolate handle is a `Future<T>` (Track A.3b's handle shape) → `all`/`race` (A.9) work over
  isolate handles unchanged.
- `concurrent { }`'s structured-scope discipline generalizes to isolate scopes (join at `}`).

So most of the *consumer* surface (await, handles, all/race, structured join) is already built; this
milestone adds the *producer* side (spawn an isolate, cross the Send boundary, channels) and the
deterministic multi-isolate scheduler underneath.

## Rough sub-slices (to be refined after sign-off)

- **I.0 — the `Send` boundary type check (E0042).** Front-end only: classify every type `Send`/`!Send`
  (structural: a container is `Send` iff its elements are), and reject a `!Send` value at a
  (not-yet-existing) boundary. Ship the *classifier* + diagnostic first, gated, so the object-model
  deferral is closed and testable independent of the runtime.
- **I.1 — `Channel<T>` value + the scheduler seam.** `SandboxScheduler` (deterministic FIFO queues +
  runnable-isolate list) behind a trait; `channel()`/`send`/`recv` (recv async). Single-isolate first
  (a channel used within one isolate — degenerate but exercises the value + async wiring in-oracle).
- **I.2 — isolate spawn + deterministic multi-isolate interleaving.** The sandbox runs N isolates
  cooperatively; message deep-copy across heaps; structured join. Fully in-oracle (deterministic).
- **I.3 — `RealScheduler` (OS threads), CLI-only.** Real parallelism behind the CLI, integration-
  tested like `RealHost`/`RealExecutor`, never in the differential.
- **I.4 — finalize:** docs (§7 alignment), deferred rows (durable queues, worker pools, app-lifetime
  `TaskScope` via DI — §7.2 framework patterns, explicitly *not* language constructs), mark complete.

## Open questions for sign-off

1. **Channel surface:** `(Sender, Receiver)` pair vs a single `Channel<T>` object? Bounded (with
   backpressure — `send` async) vs unbounded (`send` sync)? My lean: a single `Channel<T>` with async
   `recv` and, initially, unbounded `send` (backpressure later), for the smallest first cut.
2. **Isolate spawn spelling:** a free `isolate(fn, args)` returning a `Future`-handle, vs a `Worker`/
   `Isolate` type, vs generalizing `spawn` with a target. My lean: a distinct keyword/fn so the
   heap-boundary (and its cost) is *visible*, not conflated with intra-isolate `spawn`.
3. **What crosses:** confirm value-types-copied / `class`-rejected (E0042). Any exception (e.g. an
   explicitly `@shared`/atomic type) or is `!Send`-rejected absolute for v1? My lean: absolute for v1.
4. **Scope discipline:** do isolates require an owning scope (like `spawn` needs `concurrent`)? My
   lean: yes — same "nothing dangles" rule; app-lifetime workers are the §7.2 framework pattern, later.

Nothing here is built. On sign-off I'll start with **I.0** (the `Send` classifier + E0042), the one
piece that is pure front-end, in-oracle, and closes a standing object-model deferral regardless of how
the runtime questions resolve.
