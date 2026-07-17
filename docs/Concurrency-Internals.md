# Concurrency Internals

The surface — iterators, generators, `async`/`await`, isolates, channels — is on [Concurrency](Concurrency). This page is how it is built: one stackless substrate for all coroutines, and a "simulate deterministically, deploy real" split for the runtime.

## One stackless substrate

Iterators, generators, and `async`/`await` are three surfaces over **one stackless state-machine transform**, done in the shared `noeta-ir` lowering.

Why stackless is *forced*: coroutines must suspend and resume. A **stackful** model (a real per-coroutine stack) the VM could do cleanly — but the reference interpreter rides the Rust call stack and has no frame to save, yielding *two different* suspension implementations that would have to be proven observably identical. That is exactly the divergence the differential oracle exists to prevent. A **stackless** transform instead rewrites a coroutine into a state machine, so **neither backend suspends at runtime** — a coroutine is an ordinary object with a `next()`/`poll` method, and both backends run it identically by construction.

The transform's output is ordinary constructs: a closure whose captured **mutable cells** hold the state (a `$state` discriminant plus the locals live across a suspend point), driven by a `loop { match $state { … } }`. The hard compiler content is liveness across suspend points (which locals become state fields vs. stay temporaries) and mapping structured control flow to states. `yield` and `await` are both sugar onto this one primitive, differing only in the *resume driver* and the *suspend value*.

- **Lazy iterators (Track I).** `Iterable` = has `iter() -> Iterator`; `Iterator` = has `next() -> ?T` (reusing `Option`: `some(x)` = element, `none` = end). `for x in src` desugars to `it = src.iter(); loop { match it.next() { some(x) => body, none => break } }`. Adapters (`map`/`filter`/`take`/`zip`/…) are iterator-state variants wrapping a source + closure, fused so no intermediate list is built; `collect()` materializes. Iterators are reference values (calling `next()` mutates, visible to aliases) — reusing the value/reference split, no new value kind.
- **Generators (Track G).** A function containing `yield` *is* a generator (a syntactic marker, no keyword); it lowers to a closure state machine wrapped in one iterator-state variant, so generators compose with every adapter for free. Typing is clean because pull is one-directional (`next()` takes no argument): the return type is plain `Iterator<T>`, `yield e` is checked `e <: T`, a value in `return` is forbidden.
- **Async (Track A).** Rides the same state machine; the only difference is the *runtime*.

## The async runtime — an injected capability

The executor is injected as a capability, `trait Executor` (object-safe, held as `Box<dyn Executor>`), with two implementations:

| | In-oracle (deterministic) | CLI-only (real) |
|---|---|---|
| Executor | `SandboxExecutor` — single-threaded, logical time, deterministic ready-queue | `RealExecutor` — real tokio |

This is the FoundationDB / TigerBeetle model: *simulate deterministically, deploy real.* The executor owns only **time**: when a cooperative poll round makes no progress, the backend asks it to `advance` (jump to the next scheduled event) and re-polls. `await` at the async top level polls, and on `Pending` (the NaN-box `TAG_PENDING` sentinel) advances the clock and re-polls; inside an `async fn` the `.await` compiles into a poll-suspend state. Async IO leaves are request/outcome variants the sandbox performs synchronously (deterministic) and the real executor spawns on tokio. `spawn e` / `concurrent { }` register futures as tasks in a structured scope; a `spawn` with no owning scope is a compile error.

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

The real scheduler crosses threads by **faithful copy, not borrow**: no `Value`/`Rc` ever crosses a thread — only `Arc<Module>` (fully index-based, `Send + Sync`) is shared. A `Wire` marshalling enum copies arguments and results across the mpsc boundary (structs/enums keyed by shape *name* / shape-table index; a `!Send` payload is unreachable because E0042 guarantees it can't arrive). A worker builds its own VM with its own heap/host/executor, seeds globals from the parent's marshalled snapshot, runs to completion, and marshals the result back. Real parallelism is proven (two 300ms isolates finish in ~365ms). Cross-thread channels share one mutex-guarded queue polled *non-blockingly*, so a full/empty channel suspends the isolate's task rather than parking the thread — resolving the cooperative-scheduler × cross-thread-channel deadlock hazard.

> [!NOTE]
> **Deferred: zero-copy borrow-share.** A `SharedRegion` + a `shared` header bit (where `retain`/`release` are no-ops, so a shared-immutable graph is read cross-thread with no atomic ops, freed wholesale at the scope join) is fully built and miri-proven but wired into *neither* backend yet — it needs `Rc<Shape> → Arc<Shape>` first (runtime values carry a non-atomic `Rc<Shape>`, unsound to borrow across threads until shapes are thread-safe). So single-isolate programs never touch it and the corpus is byte-identical. Note also: the CLI's real-parallel path routes through the **VM only** — the reference interpreter's `Rc`-based value is `!Send`, so it stays the differential reference plus sandbox.

## Serving HTTP: inversion of control

Every capability above is **program-initiated** — the program asks, the world answers. `server.serve`
inverts that: the world initiates (a connection) and the program's handler responds. This reuses the
async substrate wholesale. Accepting a connection is an **async leaf** (like `sleep` / `fs.read_async`)
— a descriptor the executor drives (`TcpListener::accept().await` on the real host); the serve loop
polls that accept future *alongside* the in-flight handler futures each round, spawning a handler task
per connection into a **server-owned reaping set** and replying as each completes. So it is exactly
the cooperative Tier-1 model: a slow async handler yields at its `await`s while the next connection is
accepted and other handlers advance (the Node/Deno event-loop shape, on our executor). Both backends
run the identical poll order, so the interleaving is deterministic and the differential agrees.

Determinism is the **inverse of the client's pure responder**: under the sandbox the accept leaf
yields a fixed, documented **request script** and then reports the listener closed, so a served
program drives a known sequence through the handler and *terminates* in-oracle — no socket. The real
host binds a `TcpListener` and blocks. Multi-core serving (a follow-on) stays isolate-native: an
acceptor isolate hands each accepted **fd (an int)** to worker isolates over a `Channel<int>` —
intra-process fds are shared across threads, so no `SO_REUSEPORT`/`socket2` is needed.

## See also

- [Concurrency](Concurrency) — the surface these mechanisms power.
- [Memory Management](Memory-Management) — why non-atomic refcounting is sound here.
