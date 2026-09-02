# Concurrency Internals

This page is how the concurrency surface is built: one stackless substrate for every coroutine, and a "simulate deterministically, deploy real" split for the runtime. The surface itself, covering iterators, generators, `async`/`await`, isolates and channels, is on [Concurrency](Concurrency).

## One stackless substrate

Iterators, generators, and `async`/`await` are three surfaces over **one stackless state-machine transform**, done in the shared `noeta-ir` lowering (`noeta-ir/src/lower/state_machine.rs`). A coroutine becomes an ordinary object with a `next()`/`poll` method, so **neither backend suspends at runtime** and both run it identically by construction.

Stackless is what the two-backend design asks for. The reference interpreter rides the Rust call stack and has no frame to save, so a per-coroutine stack would mean a second suspension implementation alongside the VM's, and the two would have to be proven observably identical.

The transform's output is ordinary constructs. A closure's captured **mutable cells** hold the state, a `$state` discriminant plus the locals live across a suspend point, and one dispatch loop drives it:

```text
loop { match $state { 0 => …, 1 => … resume here …, } }
```

The compiler work is liveness across suspend points, which decides which locals become state fields and which stay temporaries, plus the mapping of structured control flow onto states. `yield` and `await` are the same primitive, differing in the *resume driver* and the *suspend value*.

| Surface | How it lowers |
|---|---|
| Lazy iterators | `Iterable` has `iter() -> Iterator`; `Iterator` has `next() -> ?T`, reusing `Option` so `some(x)` is an element and `none` is the end. `for x in src` desugars to `it = src.iter(); loop { match it.next() { some(x) => body, none => break } }`. |
| Iterator adapters | `map`, `filter`, `take`, `zip` and the rest are iterator-state variants wrapping a source plus a closure, fused so no intermediate list is built; `collect()` materializes. Iterators are reference values, so `next()` mutates and aliases see it. |
| Generators | A function containing `yield` *is* a generator, a syntactic marker with no keyword. It lowers to a closure state machine wrapped in one iterator-state variant, so generators compose with every adapter. Pull is one-directional, since `next()` takes no argument: the return type is plain `Iterator<T>`, `yield e` is checked `e <: T`, and a value in `return` is forbidden. |
| Async | The same state machine, differing in the *runtime* below. |

## The async runtime — an injected capability

The executor is injected as a capability, `trait Executor` (object-safe, held as `Box<dyn Executor>`, in `noeta-ext-abi/src/executor.rs`), with two implementations. The model is FoundationDB's and TigerBeetle's, *simulate deterministically, deploy real.*

| | In-oracle (deterministic) | CLI-only (real) |
|---|---|---|
| Executor | `SandboxExecutor`: single-threaded, logical time, deterministic ready-queue | `RealExecutor`: real tokio |
| `advance` | **jumps** logical time to the next deadline and no further, so a timer is due alone | sleeps until that deadline and wakes late, so every deadline the overshoot crossed is due at the same poll and the round resumes those tasks in spawn order |
| An async IO leaf | performed synchronously as a request/outcome variant | spawned on tokio |

The executor owns **time** and nothing else. When a cooperative poll round makes no progress, the backend asks it to `advance`, meaning jump to the next scheduled event, and re-polls.

The `advance` row is the one observable difference, and it decides what a program may assume about ordering. Timings that differ by less than the scheduler's own jitter carry no ordering on a real host, which is what `race`'s list-order tie-break is a rule about (`noeta-stdlib/src/task.rs`) and what `cargo test -p noeta-cli --test conformance_real_host` holds the async corpus to.

`await` at the async top level polls, and on `Pending` advances the clock and re-polls; inside an `async fn` it compiles into a poll-suspend state. `spawn e` and `concurrent { }` register futures as tasks in a structured scope, and a `spawn` with no owning scope is a compile error.

A `concurrent { }` block **inside an async fn** is split by the same desugar, so its **join is itself a poll-suspend state**:

```text
$sc = scope_begin(); …spawns/awaits…; while !scope_ready($sc) { suspend }; scope_end($sc)
```

An inner scope's tasks therefore interleave with the outer scope's siblings across the driver's rounds. The top-level `drive_future`/`join_scope` loop drives *all* open scopes each round and owns the clock advance and deadlock detection. A top-level `concurrent` runs directly in the dispatch loop and keeps its synchronous join, having no outer scope to interleave with. Sibling tasks can each hold an *open* inner scope at once, so the scope stack is **stable-indexed with tombstoning** and a handle stays valid across an out-of-order close.

The round-robin, skip-`polling`, close-on-completion **policy is value-model-neutral and mirrored** across both backends (`noeta-vm/src/scheduler.rs` ↔ `noeta-eval`), so the interleaving is byte-identical.

## Isolates and true parallelism

An **isolate** is a shared-nothing unit of execution: its own heap, communicating only by message-passing over typed channels. That is what keeps refcounts **non-atomic**, sparing them the cache-line contention atomic RC would impose, and it is the only concurrency model compatible with value semantics.

Isolates are the escalation for CPU-bound work, and async is the everyday I/O tool. Userland never sees threads, locks, or `Arc<Mutex>`.

The same deterministic/real split applies:

| Seam | Deterministic (in-oracle) | Real (CLI-only) |
|---|---|---|
| Host IO | `SandboxHost` (VFS, logical clock) | `RealHost` (disk, tokio) |
| Network | `SandboxHost` (pure request→response responder) | `RealHost` (reqwest/rustls) |
| Async | `SandboxExecutor` (logical time) | `RealExecutor` (tokio) |
| Isolates | cooperative in-VM interleave (deterministic, FIFO channels; `noeta-vm/src/isolate.rs`) | `std::thread`-per-isolate with a per-isolate tokio runtime (`noeta-host-real`) |

`std.http.client` rides both the Host split, through its `Network` capability, and the async split. A sync `client.get` performs the request through the Host, while `client.get_async` returns work the executor tickets: the real host hands over a reqwest future (`RealBody::Async`), and the sandbox resolves deterministically at spawn from the pure responder.

A program using isolates type-checks and runs *identically in the sandbox*, where it is deterministic and differential-covered, and with real parallelism on the CLI. The sandbox is observationally faithful because isolates are shared-nothing and communicate only by *copied* messages, so a single-threaded cooperative simulation matches real threads for any well-typed program.

### The `Send` classifier

Only `Send` values may cross an isolate boundary, and the **value/reference axis *is* the shareability axis**. Value types are `Send`: `struct`, primitives, `bytes`, tuples, enums, and `List`/`Map`/`Set`/`Option`/`Result` of `Send`. Reference types are `!Send`, meaning `class` with its identity and shared mutation, which is E0042 at a boundary, and `dyn`, which is conservatively `!Send`.

The checker computes this structurally, with a visited-set for recursion.

### Copy-at-the-boundary

The real scheduler crosses threads by **faithful copy** by default, with a zero-copy borrow upgrade for promotable arguments (see the note below). The only thing shared across a thread is `Arc<Module>`, which is fully index-based and `Send + Sync`; a `Value` or an `Rc` stays on its own thread. A `Wire` marshalling enum copies arguments and results across the mpsc boundary, keying structs and enums by shape *name* and shape-table index, and a `!Send` payload is unreachable there because E0042 guarantees it cannot arrive.

A worker builds its own VM with its own heap, host and executor, seeds globals from the parent's marshalled snapshot, runs to completion, and marshals the result back.

Cross-thread channels share one mutex-guarded queue, polled *non-blockingly*, so a full or empty channel suspends the isolate's task rather than parking the thread. That is what keeps a cooperative scheduler and a cross-thread channel from deadlocking each other.

**Unshippable globals.** `marshal` refuses a reference `class`, since identity cannot be copied into a fresh heap, so the parent snapshots the value-type globals and records the skipped ones as slot and type name. A `class` global an isolate never reads costs nothing. One it *does* read fails at that use with a precise E0042 naming the global, its type and the fix. The checker's E0042 classifier blocks a `class` *argument* or *result*; the global path is the one it does not see.

**Worker teardown mirrors the main heap.** A finished worker tears its own thread-local heap down exactly as the main heap's `Vm::teardown` does, ending in a backup trace from an empty root set. Refcounting alone never reclaims a cycle, so that trace is what reaps a reference cycle the worker body stranded (`a.next = b; b.next = a`) and fires each member's `__destruct`.

### Cancelling a worker

Cancelling a **cooperative task** is bookkeeping. The task is parked between polls when the request lands, so setting its `cancelled` flag means the scheduler never polls it again, which is an exact and deterministic stop.

Cancelling a **real isolate** is not bookkeeping, because the thing to stop is on the other side of a thread boundary and is running. Each worker owns an `Arc<AtomicBool>`, and `h.cancel()` stores through it and **leaves the task live**.

Only the worker can turn the task terminally cancelled, by shipping `IsolateOutcome::Cancelled` home, and that is what makes `join` honest: a `join` reports `Err(Cancelled)` for work that stopped, and `Ok(v)` for work that ran to completion after the request.

The worker notices at a **safepoint**, and the safepoints are the ones the VM already had for GC: the dispatch loop's frame transfer (`'reload`) and its taken loop back-edge (`osr_backedge!`). Every path through bytecode reaches one of them, whether it is a call, a return, or a loop that never calls, which is what makes a compute-bound isolate cancellable.

A worker is always **tier 0**, because Cranelift's `JITModule` is `!Send` and the JIT is never armed on a worker thread, so for a worker those two sites are exhaustive. The scheduler's own driving loops (`join_scope`, `drive_future_outcome`) poll as well, since a worker parked on a timer or an async leaf is running no bytecode at all.

The check is a relaxed load through an `Option<Arc<AtomicBool>>` that is `None` on every non-worker VM, so outside a worker it is one perfectly-predicted null test on a cached field.

#### The third safepoint: a JIT loop header

The same flag arms a **top-level** run (`RunOptions::cancel`), which is how `noeta test`'s per-test deadline asks an overrunning case to stop. A top-level run is JIT-served, and a loop the JIT can sustain end to end never returns to the interpreter, so the two bytecode safepoints do not cover it.

The JIT therefore carries a safepoint of its own. When the engine is built for a run that has a cancellation flag, and only then, `noeta-jit` emits at every **loop header**, already its OSR entry, an `atomic_load` of the flag and a branch to that pc's ordinary bail block. A run with no cancellation flag emits no poll at all, and a loop that already carries a bail site never ran natively anyway.

| Deliberate about the poll | Why |
|---|---|
| It deopts rather than decides | Native code hands the frame back to the interpreter at the loop header, and the interpreter's own back-edge poll makes the call one iteration later, so every rule about *when* a cancellation may be honored stays in one place. `run_destructor` lifts the flag around a destructor, and because native code can only bail, the JIT leaves a running destructor intact whatever the flag says. |
| The flag's address is a baked immediate, and the engine holds a strong `Arc` clone of it | The poll is one absolute-addressed load rather than an indirection through the VM pointer, the native↔interpreter calling convention carries nothing new, and the flag outlives every instruction that reads it even after `observe_cancel` drops the VM's own clone. |
| It is an `atomic_load` rather than a plain load | Cranelift runs at `opt_level=speed`, where an ordinary load is free to be hoisted out of the loop or folded across iterations. That is the "checks once, then never again" failure the poll exists to remove. |

The `--jit-differential` oracle runs the whole corpus a second time with a never-set flag armed (`--cancel-poll`), which puts the poll-bearing bodies under the same byte-identity, zero-residency and zero-anomaly gate as the ordinary ones.

Observing the flag raises an ordinary `Abort`. The worker unwinds the path a panic takes and then runs the same teardown a completed body does, so a cancelled worker's frame locals are destroyed and its heap returns to zero residency exactly as a joined one's. The `cancel_observed` latch distinguishes that abort from a genuine failure, and it propagates the request to every isolate this worker itself spawned, so cancelling a subtree's root stops the subtree.

A safepoint cannot preempt a worker blocked **inside a native call**, such as a pipe read or a blocking syscall, because that thread is not executing Noeta. The block's closing brace waits for it rather than abandoning it, since a thread that outlives its scope races the parent's exit-time heap teardown inside the allocator. Ending such a wait is the leaf's own job, built in the next two sections.

#### Waking a blocked worker

A worker whose only pending work is a timer parks in `RealExecutor::advance`, which sleeps real time to the earliest deadline in one call. The scheduler's cancellation poll sits at the top of the driving loop the worker has *left*, so a safepoint is the wrong instrument, the worker not being there to reach one.

The instrument is a **wake**. `CancelWake` (`noeta-ext-abi/src/executor.rs`, beside `Executor`) is a list of hooks. Whoever can block outside the interpreter registers one at startup, and the canceller fires it immediately after the flag store, so `request_isolate_cancel` is `store(true)` and then `wake()`. Registering on an already-fired wake fires the hook at once, so a worker cancelled during its own startup is roused rather than parking.

A hook's whole job is to make its block *return*. The woken party reaches its ordinary poll one round later, so `observe_cancel`'s clear-once-honored rule and `run_destructor`'s lift stay the single authority on when a cancellation may land, exactly as the JIT's loop-header poll only deopts. A spurious wake therefore costs one extra scheduler round, which the cooperative model tolerates everywhere already.

The wake a blocked party needs is whatever its own blocking primitive understands, so each party registers its own hook and `noeta-vm`, which owns the cancel, stays ignorant of all of them.

| Registers | Its hook |
|---|---|
| `RealExecutor` | `notify_one()` on a per-executor `tokio::sync::Notify` that every point `advance` can block `select!`s on, covering the timer sleep and the `JoinSet` wait for the next async IO leaf alike, so a worker awaiting `fs.read_async` or `p.read_line_async()` is roused too |
| `RealHost` | a fan-out to a condvar's `notify_all` per blocked reader, a sentinel per open stream, and a `Notify` a request in flight is raced against |
| `SandboxExecutor` | none. `Executor::set_cancel_wake` defaults to a no-op, and an `advance` that *jumps* logical time cannot block, so the oracle stays byte-identical |

The wake travels out and the executor stays put. A `RealExecutor` owns a `current_thread` tokio runtime and is built *on the worker's thread* by the `IsolateFactory`, so it cannot cross back; the parent creates the `CancelWake` at spawn beside the flag, hands the worker a clone, and the worker arms its executor before any user code runs.

A worker parked in one long sleep and one sleeping in slices therefore stop alike, at the round they wake into.

#### Making a blocking leaf return

A **blocking leaf** needs more than a wake. Its body is a `spawn_blocking` closure on the isolate's own tokio runtime, and dropping a runtime waits for every blocking task that has already started, so a woken worker unwinds and blocks *again* in teardown until the leaf returns.

An interruptible leaf therefore has to **return**, reporting `noeta_stdlib::ErrorKind::Interrupted`. Abandoning it with `Runtime::shutdown_background` would trade the wait for a thread outliving its scope, racing the heap teardown in the allocator.

`Cancellable::set_cancel` (`noeta-ext-abi/src/host.rs`) hands the host that token. It is an arm of the `Host` union, defaulting to a no-op, so a host with nothing that blocks unboundedly, meaning the deterministic sandbox, the browser and WASI, is untouched and the oracle stays byte-identical. `RealHost` registers a single hook and holds its parties weakly, so a program spawning children in a loop keeps one closure, not one per child.

The flag decides and the wake rouses. A roused party re-reads the flag and returns, and the safepoint its unwind reaches is where the cancellation is honored. `Vm::std_dispatch_error` is that seam for a leaf's error, turning `Interrupted` into the cancellation it is rather than a diagnostic, which is the difference between a worker its parent reports *cancelled* and one it reports *failed*. A condvar wake takes the waiter's own lock, so a `notify_all` cannot land in the window between the waiter's flag check and its `wait`.

`CancelSignal` bundles the flag and the wake into one object, which is what lets a top-level cancellable run, such as the `noeta test` deadline, arm its host and executor from the same token a worker isolate arms itself with.

The residual is a **file** read. It blocks in the operating system with nothing to rouse, and the only read that never returns is a FIFO or a character device.

### Channel semantics

The FIFO, bounded-capacity, rendezvous and close decisions are **value-model-neutral**, so they live once in `noeta-ext-abi::channel` (surfaced as `noeta_stdlib::channel`) and both backends call in, which holds the differential by construction.

`poll_send(capacity, buffer_len, closed, phase)` returns a `SendAction` and `poll_recv(buffer_len, closed)` returns a `RecvAction`. Each backend then performs the move over its own buffer: a `VecDeque<Value>` in the reference interpreter, and a mutex-guarded `VecDeque<Wire>` in the VM's cross-thread `ChannelCore`.

#### Rendezvous (capacity 0)

A **capacity-0 rendezvous** channel uses the buffer as a one-slot hand-off. A fresh `send` *deposits* and parks, its future carrying a `SendPhase` so it remembers, and completes once a `recv` drains the slot. The hand-off ordering is therefore observable.

#### Auto-close

**Auto-close** is keyed on **producer-task lifecycle** rather than on raw sender-value RC. A spawned task or isolate that captures a `Sender` registers a *producer hold* on the channel, counted by scanning the spawned future's captures or the isolate's args, and the hold is released when that task completes. The channel auto-closes when the last hold drops.

Keying on the task is what makes the signal timely. The enclosing `async` or top-level scope keeps a structural `Sender` alive, as a captured cell or a global, until that scope ends, which is too late to mean "no more sends".

#### Deadlock detection

A cooperative stall in the sandbox is a deterministic `E0010`.

On the real (parallel) scheduler, the root parent and every isolate worker register in a process-wide `StallRegistry`. When *every* registered scheduler is simultaneously parked at a channel stall with no timer, no pending IO and no wake during a confirm window, the deadlock is latched, so all parties unwind rather than only the detector, and each raises the same `E0010`.

> [!NOTE]
> **Zero-copy borrow-share (VM).** A `SharedRegion` plus a `shared` header bit, where `retain`/`release` are no-ops so a shared-immutable graph is read cross-thread with no atomic ops and freed wholesale at the scope join, is miri-proven and wired into the **VM**'s real-parallel path. `try_spawn_isolate_real` promotes each promotable argument graph *once* into the parent's `SharedRegion` and hands every worker a zero-copy `IsoArg::Borrowed` root, falling back to a `Wire` copy for non-promotable arguments (`noeta-vm/src/scheduler.rs`).
>
> Shapes are process-wide **interned** to a `Copy` `&'static Shape` (`Send + Sync`), so nothing holds an `Rc<Shape>` that would have to be made thread-safe.
>
> The CLI's real-parallel path routes through the **VM only**. The reference interpreter's `Rc`-based value is `!Send`, so it stays copy-only and serves as the differential reference and the sandbox.

## Serving HTTP: inversion of control

Every capability above is **program-initiated**: the program asks and the world answers. `server.serve` inverts that, so the world initiates with a connection and the program's handler responds, and it reuses the async substrate wholesale.

Accepting a connection is an **async leaf**, like `sleep` or `fs.read_async`: a descriptor the executor drives, which on the real host is `TcpListener::accept().await`. The serve loop polls that accept future *alongside* the in-flight handler futures each round, spawning a handler task per connection into a **server-owned reaping set** and replying as each completes.

That is the cooperative scheduling model unchanged. A slow async handler yields at its `await`s while the next connection is accepted and other handlers advance, which is the Node/Deno event-loop shape on this executor. Both backends run the identical poll order, so the interleaving is deterministic and the differential agrees.

Determinism is the **inverse of the client's pure responder**. Under the sandbox the accept leaf yields a fixed, documented **request script** and then reports the listener closed, so a served program drives a known sequence through the handler and *terminates* in-oracle, with no socket involved. The real host binds a `TcpListener` and blocks.

Multi-core serving (`noeta serve --parallel N`) stays isolate-native. The listener is bound **once** and each worker isolate inherits a cloned handle to it, so the kernel load-balances accepted connections across cores, leaving nothing for `SO_REUSEPORT`, `socket2` or an acceptor-side fan-out to arbitrate. See [`noeta serve`](The-CLI#noeta-serve-and---watch) for the operator-facing behavior, including how `--watch` broadcasts a hot swap across the fleet.

## See also

- [Concurrency](Concurrency) — the surface these mechanisms power.
- [Memory Management](Memory-Management) — why non-atomic refcounting is sound here.
