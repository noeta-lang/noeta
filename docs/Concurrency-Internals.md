# Concurrency Internals

The surface — iterators, generators, `async`/`await`, isolates, channels — is on [Concurrency](Concurrency). This page is how it is built: one stackless substrate for all coroutines, and a "simulate deterministically, deploy real" split for the runtime.

## One stackless substrate

Iterators, generators, and `async`/`await` are three surfaces over **one stackless state-machine transform**, done in the shared `noeta-ir` lowering.

Why stackless is *forced*: coroutines must suspend and resume. A **stackful** model (a real per-coroutine stack) the VM could do cleanly — but the reference interpreter rides the Rust call stack and has no frame to save, yielding *two different* suspension implementations that would have to be proven observably identical. That is exactly the divergence the differential oracle exists to prevent. A **stackless** transform instead rewrites a coroutine into a state machine, so **neither backend suspends at runtime** — a coroutine is an ordinary object with a `next()`/`poll` method, and both backends run it identically by construction.

The transform's output is ordinary constructs: a closure whose captured **mutable cells** hold the state (a `$state` discriminant plus the locals live across a suspend point), driven by a `loop { match $state { … } }`. The hard compiler content is liveness across suspend points (which locals become state fields vs. stay temporaries) and mapping structured control flow to states. `yield` and `await` are both sugar onto this one primitive, differing only in the *resume driver* and the *suspend value*.

- **Lazy iterators.** `Iterable` = has `iter() -> Iterator`; `Iterator` = has `next() -> ?T` (reusing `Option`: `some(x)` = element, `none` = end). `for x in src` desugars to `it = src.iter(); loop { match it.next() { some(x) => body, none => break } }`. Adapters (`map`/`filter`/`take`/`zip`/…) are iterator-state variants wrapping a source + closure, fused so no intermediate list is built; `collect()` materializes. Iterators are reference values (calling `next()` mutates, visible to aliases) — reusing the value/reference split, no new value kind.
- **Generators.** A function containing `yield` *is* a generator (a syntactic marker, no keyword); it lowers to a closure state machine wrapped in one iterator-state variant, so generators compose with every adapter for free. Typing is clean because pull is one-directional (`next()` takes no argument): the return type is plain `Iterator<T>`, `yield e` is checked `e <: T`, a value in `return` is forbidden.
- **Async.** Rides the same state machine; the only difference is the *runtime*.

## The async runtime — an injected capability

The executor is injected as a capability, `trait Executor` (object-safe, held as `Box<dyn Executor>`), with two implementations:

| | In-oracle (deterministic) | CLI-only (real) |
|---|---|---|
| Executor | `SandboxExecutor` — single-threaded, logical time, deterministic ready-queue | `RealExecutor` — real tokio |

This is the FoundationDB / TigerBeetle model: *simulate deterministically, deploy real.* The executor owns only **time**: when a cooperative poll round makes no progress, the backend asks it to `advance` (jump to the next scheduled event) and re-polls. `await` at the async top level polls, and on `Pending` (the NaN-box `TAG_PENDING` sentinel) advances the clock and re-polls; inside an `async fn` the `.await` compiles into a poll-suspend state. Async IO leaves are request/outcome variants the sandbox performs synchronously (deterministic) and the real executor spawns on tokio. `spawn e` / `concurrent { }` register futures as tasks in a structured scope; a `spawn` with no owning scope is a compile error.

A `concurrent { }` block **inside an async fn** is split by the state-machine desugar so its **join is itself a poll-suspend state**, not an in-place drive-to-completion loop: the block lowers to `$sc = scope_begin(); …spawns/awaits…; while !scope_ready($sc) { suspend }; scope_end($sc)`, where `scope_ready` is the per-poll readiness test and the suspend yields `$pending` up to the driver. So an inner scope's tasks interleave with the outer scope's siblings across the driver's rounds — the top-level `drive_future`/`join_scope` loop drives *all* open scopes each round and owns the clock advance and deadlock detection. (A top-level `concurrent`, run directly in the dispatch loop rather than a state machine, keeps its synchronous join — there is nothing outer to interleave with.) Because sibling tasks can each hold an *open* inner scope at once, the scope stack is **stable-indexed with tombstoning**: `scope_begin` appends and returns an index, `scope_end(idx)` closes that specific scope and trims trailing tombstones (the common LIFO case reclaims immediately), so handles stay valid and out-of-order closes never corrupt a sibling. The round-robin/skip-`polling`/close-on-completion **policy is value-model-neutral and mirrored** across both backends (`noeta-vm/src/scheduler.rs` ↔ `noeta-eval`), so the interleaving is byte-identical.

## Isolates and true parallelism

An **isolate** is a shared-nothing unit of execution — its own heap, communicating only by message-passing over typed channels. This is what keeps refcounts **non-atomic** (no cache-line contention, the major tax atomic RC would impose) and is the only concurrency model compatible with value semantics. Isolates are the escalation for CPU-bound work; async is the everyday I/O tool. Userland never sees threads, locks, or `Arc<Mutex>`.

The same deterministic/real split applies:

| Seam | Deterministic (in-oracle) | Real (CLI-only) |
|---|---|---|
| Host IO | `SandboxHost` (VFS, logical clock) | `RealHost` (disk, tokio) |
| Network | `SandboxHost` (pure request→response responder) | `RealHost` (reqwest/rustls) |
| Async | `SandboxExecutor` (logical time) | `RealExecutor` (tokio) |
| Isolates | cooperative in-VM interleave (deterministic, FIFO channels; `noeta-vm/src/isolate.rs`) | `std::thread`-per-isolate with a per-isolate tokio runtime (`noeta-host-real`) |

`std.http.client` rides both the Host split (its `Network` capability) and the async split: a sync `client.get` performs the request through the Host, while `client.get_async` returns work the executor tickets — the real host handing over a genuine reqwest future (`RealBody::Async`), the sandbox resolving deterministically at spawn from the pure responder.

A program using isolates type-checks and runs *identically in the sandbox* (deterministic, differential-covered) and with real parallelism on the CLI. The sandbox is observationally faithful because isolates are shared-nothing and communicate only by *copied* messages — a single-threaded cooperative simulation cannot differ from real threads for any well-typed program.

### The `Send` classifier

Only `Send` values may cross an isolate boundary, and the **value/reference axis *is* the shareability axis**: value types (`struct`, primitives, `bytes`, tuples, enums, and `List`/`Map`/`Set`/`Option`/`Result` of `Send`) are `Send`; reference types (`class` — identity, shared mutation) are `!Send` (E0042 at a boundary); `dyn` is conservatively `!Send`. The checker computes this structurally with a visited-set for recursion.

### Copy-at-the-boundary

The real scheduler crosses threads by **faithful copy** by default (with a zero-copy borrow upgrade for promotable arguments — see the note below): no `Value`/`Rc` ever crosses a thread — only `Arc<Module>` (fully index-based, `Send + Sync`) is shared. A `Wire` marshalling enum copies arguments and results across the mpsc boundary (structs/enums keyed by shape *name* / shape-table index; a `!Send` payload is unreachable because E0042 guarantees it can't arrive). A worker builds its own VM with its own heap/host/executor, seeds globals from the parent's marshalled snapshot, runs to completion, and marshals the result back. Real parallelism is proven (two 300ms isolates finish in ~365ms). Cross-thread channels share one mutex-guarded queue polled *non-blockingly*, so a full/empty channel suspends the isolate's task rather than parking the thread — resolving the cooperative-scheduler × cross-thread-channel deadlock hazard.

**Unshippable globals.** `marshal` refuses a reference `class` (identity cannot be copied into a fresh heap — the value/reference axis *is* the shareability axis), so the parent snapshots only value-type globals and records the skipped ones (slot → type name). A `class` global an isolate never reads costs nothing; one it *does* read fails at that use with a precise E0042 that names the global + type + fix, rather than a confusing "cannot find `x`" or a silently split duplicate. (The checker's E0042 classifier already blocks a `class` *argument*/*result*; this is the *global* path it does not see.)

**Worker teardown mirrors the main heap.** When a worker finishes, it tears its own thread-local heap down exactly like the main heap's `Vm::teardown`: a pre-teardown trace from the still-bound globals, then global destruction in reverse order, then a backup trace from an empty root set — so a reference cycle the worker body stranded (`a.next = b; b.next = a`) is reaped and each member's `__destruct` fires, instead of leaking until the thread dies. Refcounting alone never reclaims a cycle, so without this pass the worker's cycle garbage (and its destructors) were lost.

### Cancelling a worker

Cancelling a **cooperative task** is bookkeeping: the task is parked between polls when the request lands, so setting its `cancelled` flag means the scheduler never polls it again, which is an exact and deterministic stop. Cancelling a **real isolate** cannot be bookkeeping, because the thing to stop is on the other side of a thread boundary and is running. Each worker therefore owns an `Arc<AtomicBool>`; `h.cancel()` stores through it and, crucially, **leaves the task live**. Only the worker can turn the task terminally cancelled, by shipping `IsolateOutcome::Cancelled` home — which is what makes `join` honest. (Latching "cancelled" at the moment of asking is exactly the bug this replaced: `join` reported `Err(Cancelled)` for work that then ran to completion, and the `concurrent` block returned over a thread that was still executing.)

The worker notices at a **safepoint**, and the safepoints are the ones the VM already had for GC: the dispatch loop's frame transfer (`'reload`) and its taken loop back-edge (`osr_backedge!`). Between them, every path through bytecode — a call, a return, a loop that never calls — reaches a check, which is what makes a compute-bound isolate genuinely cancellable rather than merely reported as such. A worker is always **tier 0** (Cranelift's `JITModule` is `!Send`, so the JIT is never armed on a worker thread), so for a worker those two sites are exhaustive. The scheduler's own driving loops (`join_scope`, `drive_future_outcome`) poll as well, since a worker parked on a timer or an async leaf is running no bytecode at all.

The check is a relaxed load through an `Option<Arc<AtomicBool>>` that is `None` on every non-worker VM, so outside a worker it is one perfectly-predicted null test on a cached field. Measured over a 40-million-iteration tier-0 loop with the flag armed (the worst case), best-of-five went from 2578 ms to 2590 ms; the JIT-served main path with the flag absent went from 348 ms to 355 ms. Both are inside the run-to-run spread on a loaded machine.

#### The third safepoint: a JIT loop header

The same flag arms a **top-level** run (`RunOptions::cancel`), which is how `noeta test`'s per-test deadline asks an overrunning case to stop — and a top-level run *is* JIT-served, so the two bytecode safepoints are not exhaustive there. A loop the JIT can sustain end to end never returns to the interpreter at all, so for a while a cancellable run simply declined on-stack replacement to stay reachable: precise, cheap to implement, and a 10× tax on exactly the loops it applied to (a 200M-iteration counting loop, 6.53 s bounded against 0.64 s unbounded).

The JIT now carries the safepoint itself. When — and only when — the engine is built for a run that has a cancellation flag, `noeta-jit` emits at every **loop header** (the target of a backward branch, which is already its OSR entry) an `atomic_load` of the flag and a branch to that pc's ordinary bail block. Three things about that shape are deliberate:

- **The poll decides nothing; it deopts.** Native code never unwinds on the flag — it hands the frame back to the interpreter at the loop header, and the interpreter's own back-edge poll makes the call one iteration later. Every rule about *when* a cancellation may be honored therefore stays in one place. Most usefully, `run_destructor` lifts the flag around a destructor, and because native code can only bail, the JIT cannot truncate a destructor no matter what the flag says while one runs.
- **The flag's address is a baked immediate, not an ABI field.** It reaches codegen the same way the frame template and the inline-cache slots do — a constant the engine knows at construction — so nothing in the native↔interpreter calling convention changed, and the poll is one absolute-addressed load rather than an indirection through the VM pointer. The engine keeps a strong `Arc` clone, declared after the module it bakes code into, so the flag outlives every instruction that reads it even though `observe_cancel` drops the VM's own clone the moment a request is honored.
- **It is an `atomic_load` rather than a plain load** because Cranelift runs at `opt_level=speed`: an ordinary load would be free to be hoisted out of the loop or folded across iterations, which is precisely the "checks once, then never again" bug the poll exists to remove.

Measured under `noeta test` with the decline removed (median of five, quiet machine): the 200M-iteration counting loop is **0.76 s bounded against 0.66 s unbounded**, where it was 6.53 s against 0.64 s — so the poll costs roughly half a nanosecond per iteration and the ten-fold penalty is gone. A 60M-iteration loop that already carried a bail site measured 5.61 s bounded / 5.66 s unbounded against 5.49 / 5.57 before: unchanged, as expected, since it never ran natively in the first place. A run with no cancellation flag emits no poll at all, so its generated code is byte-identical to the pre-poll compiler. The `--jit-differential` oracle runs the whole corpus a second time with a never-set flag armed (`--cancel-poll`), which puts the poll-bearing bodies under the same byte-identity, zero-residency and zero-anomaly gate as the ordinary ones.

Observing the flag raises an ordinary `Abort`. The worker unwinds through the same path a panic takes — every live register released, every frame local's destructor fired — and then runs the same teardown a completed body does, so a cancelled worker's heap returns to zero residency exactly like a joined one. The `cancel_observed` latch is what distinguishes the resulting abort from a genuine failure, and it also propagates the request to every isolate this worker itself spawned, so cancelling a subtree's root stops the subtree.

What this cannot do is preempt a worker blocked **inside a native call**: a pipe read, a socket read, a blocking syscall. That thread is not executing Noeta, so no safepoint comes around, and the block's closing brace waits for it. Waiting is the deliberate choice over abandoning: a worker owns its own heap, its host handles, and its channel endpoints, and letting it outlive its scope was not merely untidy — the abandoned thread raced the parent's exit-time heap teardown and segfaulted in the allocator, reproducibly. Bounding a hostile native call belongs to the operation (a deadline on the read), not to the cancel around it; making host IO interruptible is the open follow-up.

A **long `sleep`** is the same shape in miniature, and worth naming separately because it looks cancellable and is not. A worker whose only pending work is a timer parks in `RealExecutor::advance`, which sleeps real time to the earliest deadline in one call; the scheduler's cancellation poll sits at the top of the driving loop, so it comes around when that sleep returns. Measured: a worker in `sleep(3000).await`, cancelled 200 ms in, stopped 2.8 s later — the remaining sleep — while the same wait written as 5 ms slices stopped within a millisecond. `advance`'s sleep already `select!`s on a `tokio::sync::Notify`, so the fix has a shape: hand each worker's executor a notifier the parent's `request_isolate_cancel` fires. It needs the notifier to travel *back* from the worker at startup, which is a second channel and an `Executor`-trait method, so it is a follow-up rather than part of this.

### Channel semantics

The FIFO / bounded-capacity / rendezvous / close decision is **value-model-neutral**, so it lives once in `noeta-ext-abi::channel` (surfaced as `noeta_stdlib::channel`) and both backends call in — the differential holds by construction. `poll_send(capacity, buffer_len, closed, phase)` returns a `SendAction`, `poll_recv(buffer_len, closed)` a `RecvAction`; each backend then performs the move over its own buffer (`VecDeque<Value>` in the reference interpreter, a mutex-guarded `VecDeque<Wire>` in the VM's cross-thread `ChannelCore`).

#### Rendezvous (capacity 0)

A **capacity-0 rendezvous** channel uses the buffer as a one-slot hand-off: a fresh `send` *deposits* and parks (its future carries a `SendPhase` so it remembers), completing only once a `recv` drains the slot — the hand-off ordering is therefore observable.

#### Auto-close

**Auto-close** is keyed on **producer-task lifecycle**, not raw sender-value RC: a spawned task/isolate that captures a `Sender` registers a *producer hold* on the channel (counted by scanning the spawned future's captures, or the isolate's args), released when that task completes; when the last hold drops the channel auto-closes. This sidesteps a drop-precision limitation — the enclosing `async`/top-level scope keeps a structural `Sender` alive (as a captured cell or a global) until it ends, too late to signal "no more sends".

#### Deadlock detection

A cooperative stall in the sandbox is a deterministic `E0010`. On the real (parallel) scheduler, the root parent and every isolate worker register in a process-wide `StallRegistry`; when *every* registered scheduler is simultaneously parked at a channel stall with no timer, no pending IO, and no wake during a confirm window, the deadlock is latched (so all parties unwind, not just the detector) and each raises the same `E0010` — instead of spinning forever.

> [!NOTE]
> **Zero-copy borrow-share (VM).** A `SharedRegion` + a `shared` header bit (where `retain`/`release` are no-ops, so a shared-immutable graph is read cross-thread with no atomic ops, freed wholesale at the scope join) is miri-proven and wired into the **VM**'s real-parallel path: `try_spawn_isolate_real` promotes each promotable argument graph *once* into the parent's `SharedRegion` and hands every worker a zero-copy `IsoArg::Borrowed` root, falling back to a `Wire` copy only for non-promotable arguments (`noeta-vm/src/scheduler.rs`). The old blocker is gone — shapes are process-wide **interned** to a `Copy` `&'static Shape` (`Send + Sync`), so there is no `Rc<Shape>` to make thread-safe. Note also: the CLI's real-parallel path routes through the **VM only** — the reference interpreter's `Rc`-based value is `!Send`, so it stays copy-only and remains the differential reference plus sandbox.

## Serving HTTP: inversion of control

Every capability above is **program-initiated** — the program asks, the world answers. `server.serve` inverts that: the world initiates (a connection) and the program's handler responds. This reuses the async substrate wholesale. Accepting a connection is an **async leaf** (like `sleep` / `fs.read_async`) — a descriptor the executor drives (`TcpListener::accept().await` on the real host); the serve loop polls that accept future *alongside* the in-flight handler futures each round, spawning a handler task per connection into a **server-owned reaping set** and replying as each completes. So it is exactly the cooperative scheduling model: a slow async handler yields at its `await`s while the next connection is accepted and other handlers advance (the Node/Deno event-loop shape, on our executor). Both backends run the identical poll order, so the interleaving is deterministic and the differential agrees.

Determinism is the **inverse of the client's pure responder**: under the sandbox the accept leaf yields a fixed, documented **request script** and then reports the listener closed, so a served program drives a known sequence through the handler and *terminates* in-oracle — no socket. The real host binds a `TcpListener` and blocks. Multi-core serving (`noeta serve --parallel N`) stays isolate-native: the listener is bound **once** and each worker isolate inherits a cloned handle to it, so the kernel load-balances accepted connections across cores — no `SO_REUSEPORT`, no `socket2`, and no acceptor-side fan-out to arbitrate. See [`noeta serve`](The-CLI#noeta-serve-and---watch) for the operator-facing behavior, including how `--watch` broadcasts a hot swap across the fleet.

## See also

- [Concurrency](Concurrency) — the surface these mechanisms power.
- [Memory Management](Memory-Management) — why non-atomic refcounting is sound here.
