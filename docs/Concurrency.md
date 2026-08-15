# Concurrency

The language has lazy iterators, generators, `async`/`await` with structured concurrency, and true-parallel isolates communicating over typed channels — all built on one stackless state-machine substrate (see [Concurrency Internals](Concurrency-Internals) for how). This page is the surface.

Here is the heart of it in one complete program — two tasks spawned, both in flight at once, joined with `.await`:

```noeta
use std.task.{sleep}

async fn fetch_user(id: int): string {
    sleep(20).await                     // stand-in for a real request
    return "user-${id}"
}

concurrent {
    a = spawn fetch_user(1)             // both requests start now…
    b = spawn fetch_user(2)
    echo "requests in flight"
    echo a.await                        // …and run while we wait
    echo b.await
}
echo "done"
```

```console
$ noeta run fetch.noe
requests in flight
user-1
user-2
done
```

An `async fn` suspends instead of blocking, `spawn` schedules a call as a task inside a `concurrent` scope, and `.await` waits for a result. The rest of this page builds the full picture: iterators and generators first (the same state-machine substrate in synchronous form), then async/await, structured scopes, cancellation, isolates, and channels.

## Lazy iterators

`xs.iter()` produces an `Iterator<T>` — a **reference** value with a shared cursor. Adapters are lazy and fuse (no intermediate lists are built); a terminal like `.collect()` or `.sum()` drives them.

```noeta
echo [1, 2, 3, 4, 5].iter()
    .map(fn(n) => n * 10)
    .filter(fn(n) => n > 15)
    .take(2)
    .collect()                 // [20, 30]
```

Adapters: `.map(f)`, `.filter(pred)`, `.take(n)`, `.drop(n)`, `.chain(it)`, `.enumerate()`, `.zip(it)`. Terminals: `.next()` (→ `?T`), `.collect()`, `.count()`, `.sum()`. A `for` loop drives an iterator directly (streaming via `next()`). See [Standard Library](Standard-Library#iterators) for the full list.

## Generators — `yield`

A function that contains `yield` **is** a generator, and must return `Iterator<T>`. `yield expr` produces the next element; a bare `return;` ends iteration. `yield` may appear inside `if`/`while`/`for` — it is flattened into a state machine — and generators may be infinite (consumed lazily):

```noeta
fn naturals(): Iterator<int> {
    mut n = 0
    while true {
        yield n
        n = n + 1
    }
}
echo naturals().take(5).collect()   // [0, 1, 2, 3, 4]
```

`yield` outside a generator (or inside a closure passed to a builtin) is E0039; a value in a generator's `return` is E0039.

## Async / await

- **`async fn f(): T`** — calling it yields a `Future<T>` without running the body.
- **`expr.await`** — postfix; suspends until the future resolves, then unwraps it. Awaiting a non-future is E0040. Top-level `await` is allowed.

```noeta
use std.task.{sleep}
async fn nap(name: string, ms: int): int {
    echo "${name} start"
    sleep(ms).await
    echo "${name} end"
    return ms
}
```

`.await` is legal in every **value position**. An unconditional mid-expression `.await` (a call argument, an operand, a list element) is hoisted to statement position automatically, left to right. A **conditionally-evaluated** `.await` — the right side of `&&`/`||`, a `??` fallback, or a `match`/`if…then…else` arm body — is rewritten into control flow so it runs exactly when the surrounding expression would evaluate it (laziness is preserved: a `??` fallback is awaited only when the value is `none`/`Err`; a `match` arm only when it is selected).

What stays E0040: `.await` inside a **closure** (function coloring — a closure is a fresh callable, not the enclosing async context), and `.await` in a **condition or loop head** — an `if`/`while` condition or a `for` iterable — which cannot be hoisted without changing when the head evaluates. Awaiting a non-future is also E0040.

| Position | `.await` allowed? |
|---|---|
| Statement value (`x = f().await`, `return`, `echo`, `?`) | ✅ |
| Unconditional sub-expression (call arg, operand, element) | ✅ (hoisted) |
| `&&` / `||` right operand | ✅ (guarded) |
| `??` fallback | ✅ (guarded, lazy) |
| `match` / `if…then…else` arm body | ✅ (guarded) |
| `if` / `while` condition, `for` iterable (heads) | ❌ E0040 |
| Inside a closure | ❌ E0040 |

### `async` methods and traits

A method may be `async`, including a trait method — required or defaulted. The rule is the one above with nothing added: calling it produces a `Future<T>`, and that is true of *every* way the receiver is reached — a concrete type, a `<F: Fetcher>` bound, a `dyn Fetcher` trait object, or a `dyn` narrowed with `x is dyn Fetcher`.

```noeta
use std.task.{sleep}

trait Fetcher {
    async fn fetch(url: string): string
}

struct Http {
    impl Fetcher {
        pub async fn fetch(url: string): string {
            sleep(1).await
            return "body:" ~ url
        }
    }
}

async fn via_dyn(f: dyn Fetcher): string { return f.fetch("one").await }
echo via_dyn(Http {}).await   // body:one
```

What makes that uniform typing honest is that an implementation must match its trait's `async`-ness: a plain `fn` satisfying an `async` trait method — or an `async fn` satisfying a synchronous one — is E0015. Forgetting the `.await` is then an ordinary E0007 (`expected string, found Future<string>`) wherever the call is reached from.

## Structured concurrency

A `concurrent { … }` scope runs tasks concurrently and joins them at the closing brace. Inside it:

- **`spawn expr()`** schedules a future as a task, yielding a handle you can `.await` (or `.cancel()` / `.join()` — see [Cancellation](#cancellation)).
- **`isolate f(args)`** runs in a fresh isolate (own heap, true parallelism); its arguments and result must be `Send` (see below). Like `spawn`, it takes a **future** — so `f` must be an `async fn`; handing it a synchronous call is E0041.

```noeta
use std.task.{sleep, all}
async fn work(name: string, ms: int): int {
    sleep(ms).await
    return ms
}

concurrent {
    hs = [spawn work("a", 2), spawn work("b", 1), spawn work("c", 3)]
    xs = all(hs)                        // awaits all; results in input order
    echo "all=" ~ xs.join(",")         // all=2,1,3
}
```

### Combinators

| Combinator | Behavior |
|---|---|
| `all(list)` | Awaits every future concurrently; returns results in **input order**. |
| `race(list)` | Returns the first result at once; every loser is cancelled. The block's closing brace still joins the losers — see [Cancellation](#cancellation). |
| `map_bounded(items, n, f)` | Applies async `f` to each item with at most `n` in flight; results in item order. |

### Nested `concurrent` interleaves

A `concurrent { … }` block opened **inside a spawned task's own body** does not run as one atomic step: the inner scope's tasks interleave with the outer scope's siblings. Two sibling tasks that each open their own `concurrent` therefore run interleaved, not one-after-the-other, and a nested block can finish while a sibling's block is still open. The structured guarantee is unaffected by any of this — every block joins all of its tasks before it returns. For the scheduler mechanics behind the interleaving, see [Concurrency Internals](Concurrency-Internals).

### Cancellation

A task handle (what `spawn`/`isolate` return — itself a `Future<T>`) can be cancelled. `cancel` is a **request** and `join` is the **report**: you ask the work to stop, and the join tells you whether it actually did. That split is the whole contract, and it is what keeps cancellation honest for a `spawn`ed task and a real parallel isolate alike, even though they stop for completely different reasons.

| Operation | Behavior |
|---|---|
| `h.cancel(): void` | Requests a stop — idempotent, and a **no-op on an already-completed task** (its result is preserved). It returns nothing because at the moment you ask, nothing is yet known; `join` is where you find out. |
| `h.join(): Result<T, Cancelled>` | Drives the task to a terminal state and reports it: `Ok(v)` if the body produced a value, `Err(Cancelled)` if it stopped without producing one. The explicit, cancel-aware way to await. |
| `h.await: T` | Unchanged for the common case. On a task that **did** stop cancelled it fails loudly (`E0056`) — that task never produces a value, so awaiting it is a bug. Cancel-aware code uses `h.join()` instead. |

```noeta
use std.task.{sleep}
async fn work(): int {
    sleep(10).await
    return 5                            // never reached once cancelled
}

concurrent {
    h = spawn work()
    sleep(1).await                      // let `work` reach its suspension
    h.cancel()                          // request the stop
    echo match h.join() {
        Ok(v)  => "done=" ~ v,
        Err(_) => "cancelled",          // ← taken
    }
}
```

**Where the stop happens.** A **task** (`spawn`, or a `race` loser) is already parked between polls when the request lands, so it stops exactly there: its last `.await`, and the code past that point never runs. A **real isolate** is an OS thread that is genuinely running, so it stops at its next **safepoint** — a call, a return, a loop iteration, or a round of its own scheduler. Safepoints are dense enough that a compute-bound isolate with no suspension point anywhere in it is still cancellable; a 40-million-iteration arithmetic loop cancelled 200 ms in stops within milliseconds rather than running its remaining three-and-a-half seconds.

**What cancellation does not promise.** Each of these is a deliberate limit, not an omission.

- **It does not undo work already done.** A cancelled task keeps every file it wrote, every message it sent, every row it inserted. `Err(Cancelled)` means "this produced no value", not "this never happened" — a caller that needs the effects reverted has to revert them.
- **A request that arrives too late is not a cancellation.** If the body finished before the request was noticed, `join` reports `Ok(v)`. It never claims work that ran to completion was cancelled.
- **It does not preempt a native call mid-flight.** An isolate inside the host is not executing Noeta and reaches no safepoint, so the call ends *itself* and the isolate stops at the safepoint just after. Which calls can do that is the list below.

**A long `sleep` is cancellable.** A worker parked in `sleep(3000).await` is not running Noeta either — its clock advance is a single real sleep — so the request is delivered with a **wake** that ends that sleep, and the worker stops at the round it wakes into: measured, a 3 s sleep cancelled 200 ms in ends the run at 0.21 s. Nothing is asked of your code: `while w < ms { sleep(5).await; w = w + 5 }` and `sleep(ms).await` stop alike, so there is no reason to sleep in slices.

**Blocking work stops where it can.** The wait a cancel reaches is not only the scheduler's: a run that is stopping tells its host so, and every place the host would otherwise wait on something that may never come ends its wait and reports that the run is stopping. Four of them, all the cases where waiting is unbounded by nature:

| Waiting on | Ends at the cancel |
|---|---|
| `p.read_line()` / `p.read(n)` / `p.read_err_line()` — a child that has not spoken | yes |
| `p.wait()` — a child that has not exited | yes |
| `http.fetch` / `http.try_fetch` — a request in flight | yes; the recoverable door reports `kind() == "interrupted"`, which `retryable()` answers false for |
| a `stream(...)` body's next frame — a connection that has gone quiet | yes |

Each of these stops *distinguishably*. A read that ends because the run is stopping is not an end of stream, and does not answer `none`: a child you have stopped listening to and a child that has stopped talking are different things, and code that treats the first as the second walks past the loop it was supposed to stop in.

**A file read is the exception, and deliberately so.** `fs.read` and its `_async` twin are not interruptible: they block in the operating system, and the only file read that never returns is a FIFO or a character device — which is not really a file. Put a deadline on the operation rather than a cancel around it. The `_async` twins are otherwise as cancellable as their blocking forms: awaiting `p.read_line_async()` frees the scheduler to run your other tasks *and* the read itself ends at the cancellation, so nothing is left holding the isolate open.

**Cancellation and the closing brace.** A `concurrent` block joins everything it spawned, and a cancelled member is no exception: the block waits for it to actually stop before returning. This is the load-bearing half of structured concurrency, and it is why a cancelled isolate is joined rather than abandoned — the alternative is a thread that outlives its scope, still holding its heap and its handles, still writing to the world the program thinks it has finished with.

**`join` vs `await`.** `join` is the pairing for cancellable work — it keeps the typed cancelled outcome in the language's ordinary `Result`/`match` vocabulary, while plain `await` stays `T` for the overwhelmingly-common uncancelled path and fails loudly (`E0056`) rather than silently if it ever meets a task that stopped cancelled (Noeta has no exceptions to catch, so a silent zero would be unsound). `cancel`/`join` are offered on every `Future<T>` because a handle *is* a `Future<T>`; on a bare (never-spawned) future `cancel` is a harmless no-op and `join` equals `Ok(future.await)`.

The `Cancelled` marker is a payload-free prelude enum — matchable (`Err(Cancelled.Cancelled)`, or just `Err(_)`), and `Send`. Cancelling a producer task composes with channels: its `Sender` **producer hold** releases when its future is reclaimed at the scope's close, auto-closing the channel exactly as a completed producer's would. Cancelling an isolate that itself spawned isolates cancels those too — a subtree stops together. Either way a cancelled task's captured locals' destructors run when its future is reclaimed at the scope's close, so cancelling frees no differently than a normal join (residency stays 0).

## Isolates and `Send`

An **isolate** is a shared-nothing unit of execution — its own heap, communicating only by message. `isolate f(args)` runs `f` on a fresh isolate with real parallelism.

Only `Send` values may cross an isolate boundary, and the **value/reference axis is the shareability axis**:

- **`Send`**: value types — primitives, `bytes`, tuples, structs of `Send` fields, enums, and `List`/`Map`/`Set`/`Option`/`Result` of `Send`.
- **`!Send`**: reference types (`class` — they have identity and shared mutation) and `dyn`.

Sending a `!Send` value across an isolate is E0042.

The rule also covers **globals**, not just a call's arguments and result: an isolate runs in a fresh heap and snapshots the module's value-type globals by copy, but a reference `class` global has identity and cannot be copied across — so it is **not** shared. A worker that reads such a global fails at that use naming the global, its type, and the fix (make it a value `struct`, or pass the value-type data it holds as arguments) rather than silently observing a stale duplicate. A `class` global an isolate never reads is fine — only a read triggers the error.

### What an isolate prints

An isolate's `echo` and `io.out`/`io.errln` are part of the program's output like anything else — an isolate is shared-nothing, not silent. Because it has its own heap and its own output buffers, what it writes is **handed back with its result**, and that fixes where it appears:

> **An isolate's output arrives as one contiguous block, at the point the awaiting code joins it.**

Three consequences, and they are the whole contract:

- **Within a block, the isolate's own order is exact.** Two `echo`s in a worker are two adjacent lines, in the order the worker wrote them. Nothing else can appear between them.
- **The block lands where you joined it** — at the `.await`, in the *awaiting* code's order. So a program that starts and joins its isolates one at a time has a completely determined transcript, and it reads top to bottom:

```noeta
async fn work(name: string): int { echo "worker " ~ name; return 1 }

async fn both(): int {
    mut n = 0
    echo "before"
    concurrent { a = isolate work("a"); n = n + a.await }
    echo "between"                                          // "worker a" has already appeared
    concurrent { b = isolate work("b"); n = n + b.await }
    echo "after"
    return n
}
```

- **Isolates running at the same time have no order between them,** and none is invented. Two workers joined out of one `concurrent` block appear in the order they finish, which is thread scheduling — reproducible only if you make it so. Interleaving their lines by timestamp was the alternative and is deliberately not what happens: it would need a wall clock the deterministic sandbox does not have, and it would shred each worker's own transcript to fabricate a global order that never existed. Grouping keeps the order that is real and declines to invent the one that is not.

On a live run (`noeta run`, `noeta serve`) output streams as it is produced, so a worker's completed lines reach the terminal immediately rather than waiting for the join; only an unterminated last line waits. Where output is *collected* rather than streamed — `noeta test`, `--json`, an embedder reading the run's result — the block rule above is what you get.

## Channels

A bounded, typed channel connects tasks or isolates:

```noeta
async fn produce(tx: Sender<int>): void {
    for i in 0..5 { tx.send(i).await }
    tx.close()
}

async fn consume(rx: Receiver<int>): int {
    mut total = 0
    while true {
        match rx.recv().await {
            some(v) => { total = total + v },
            none    => { return total },        // channel closed and drained
        }
    }
    return total
}

(tx, rx) = channel::<int>(2)            // capacity-2 channel
concurrent {
    spawn produce(tx)
    h = spawn consume(rx)
    echo h.await                        // 10
}
```

- `channel::<T>(cap)` returns `(Sender<T>, Receiver<T>)`; both ends are `Send`.
- `tx.send(v).await` — async; applies backpressure when the buffer is full.
- `tx.close()` — marks the channel closed (idempotent — closing twice is harmless).
- `rx.recv().await` — `some(v)` while values remain, `none` once closed and drained.

### Channel semantics

| Behavior | Rule |
|---|---|
| **Buffered** (`cap >= 1`) | `send` completes as soon as the message is enqueued into an open buffer with room; a full buffer applies backpressure (the `send` parks until a `recv` frees a slot). |
| **Rendezvous** (`cap == 0`) | A direct hand-off: `send` parks until a receiver *takes* the message, and `recv` parks until a sender offers one — the send completes **after** the receive (observable ordering; the sender never runs ahead). |
| **Auto-close** | When every spawned task/isolate that holds a `Sender` for a channel has completed, the channel **closes on its own** — receivers drain the buffer, then observe `none` instead of blocking forever. A `Sender` kept only by a long-lived enclosing scope (never handed to a producer) does not trigger this; use `tx.close()` for that. |
| **Explicit close** | `tx.close()` still works and is idempotent; it composes with auto-close (whichever happens first closes the channel). |
| **Deadlock** | A channel that can make no progress — every party blocked on channel ops with no live counterparty, no timer, and no pending IO — is a deterministic deadlock: the sandbox catches it as `E0010`, and the real (parallel) scheduler raises the same `E0010` rather than spinning. |

## Streaming I/O

Some sources produce values *over time* rather than all at once. The ones in the standard library share the channel shape above — an awaited read yielding `none` when the source is finished — so one `while` loop drains any of them:

| Source | Read | Ends when |
|---|---|---|
| `Receiver<T>` (a channel) | `rx.recv().await` → `?T` | the channel is closed and drained |
| `FrameStream` (an HTTP response body) | `stream.recv().await` → `?Frame` | the body ends |
| `Socket` (a websocket session) | `sock.recv().await` → `?string` | the peer closes |
| `Process` (a child's output) | `p.read_line_async().await` → `?string` | the child's output ends |

```noeta
use std.http.client
use std.http.{Framing, Frame, HttpError}

// `Result<void, HttpError>` rather than `void`, because the body uses `?`: the operator early-returns
// the failure, so the signature has to be able to carry one — see [`?` — propagate a
// failure](Error-Handling#--propagate-a-failure).
async fn tokens(body: string): Result<void, HttpError> {
    api = client.new("https://api.example.com")
    opened = client.stream(api.prepare("post", "/v1/chat", body), Framing.Sse)?
    // Check the head before draining the body — see below.
    stream = opened.error_for_status()?
    mut going = true
    while going {
        next = stream.recv().await
        if next == none {
            going = false
        } else {
            f: Frame = next ?? Frame { event: "", data: "", id: "", retry: none }
            echo f.data
        }
    }
    return Ok()
}
```

### The response head

A streamed response has the same two halves as a buffered one — a head and a body — and only the body arrives over time. `FrameStream` therefore carries the head, readable **before** the first `recv()`: `status()`, `ok()`, `header(name)`, and the opt-in `error_for_status()` mirror the same methods on `Response`.

Check it. A rate-limited provider answers a streaming request with `429` and a bare JSON error document, which is not an event stream — so `Framing.Sse` correctly cuts it into **zero** frames, and a reader that only drains cannot tell a rate limit from a model with nothing to say. The head is also where the actionable part lives: `stream.header("retry-after")` tells a backoff loop how long to wait, and a provider's `x-ratelimit-*` headers report the remaining budget.

The split follows the one-shot verbs exactly. Opening a stream returns `Err` only when the request never got off the ground — a transport failure — so plain `?` keeps its single meaning, and `error_for_status()` is how a caller opts a non-2xx into the same short-circuit.

A `FrameStream` is a **handle** on a single consumable body: copies alias it, and it belongs to the task that opened it. A **`Frame`, by contrast, is a value struct** — so by the rule in [Isolates and `Send`](#isolates-and-send) it is `Send`. That is deliberate rather than incidental: it lets one task own the body while others receive frames over a channel, which is the natural shape of a streaming pipeline.

```noeta ignore
concurrent {
    spawn read_into(stream, tx)     // one task owns the FrameStream
    spawn forward(rx)               // others receive Frames over a channel
}
```

Backpressure runs end to end: the real host's reader holds a bounded number of decoded frames and then stops reading the socket, so a slow consumer slows the *server* down instead of growing memory.

Serving a stream is the mirror image. `server.sse(handler)` runs `handler(sink)` as a session and `sink.send(frame)` pushes to the client, exactly as `server.websocket` does for a socket — both are ordinary in-flight handlers to the serve loop, so a long-lived session interleaves with other requests rather than blocking them.

## Streaming a subprocess

A spawned child (`os.spawn` / `os.try_spawn` — see [Error Handling](Error-Handling#aborting-and-recoverable-doors) for which door to use) has both a **blocking** and an **awaitable** form of every read, and the choice matters more here than anywhere else in the standard library.

| Blocking | Awaitable twin | Reads |
|---|---|---|
| `p.read_line(): ?string` | `p.read_line_async(): Future<?string>` | the next line of the child's stdout |
| `p.read_err_line(): ?string` | `p.read_err_line_async(): Future<?string>` | the next line of its stderr, on its own cursor |
| `p.read(n): ?string` | `p.read_async(n): Future<?string>` | up to `n` characters of stdout |
| `p.wait(): ExecResult` | `p.wait_async(): Future<ExecResult>` | the child's exit |

The blocking reads park the **whole isolate**, not just the calling task: a sibling task spawned as a watchdog does not get to run while a `read_line` waits on a child that has not spoken yet. So a synchronous read cannot be bounded from inside the language — the only escapes are killing the child or standing up a second isolate to kill it by pid.

The awaitable twin makes a bounded read ordinary. `race` returns the first result, so pair the read with a deadline that resolves to the same type:

```noeta
use std.{os, task}
use std.os.Process

async fn deadline(ms: int): ?string {
    task.sleep(ms).await
    return none
}

async fn first_line(p: Process, ms: int): ?string {
    return task.race([p.read_line_async(), deadline(ms)])
}
```

Both reads share one cursor per stream, so an awaited read and a blocking one interleave on the same position — `p.read_async(3).await` then `p.read(5)` continues where the first stopped.

A child's stdout and stderr are drained continuously in the background, so a chatty child never deadlocks on a full pipe while you supervise it, and `p.wait()` still returns the *whole* captured output regardless of how much was streamed.

## Determinism

In the sandbox executor (used for the differential oracle) time is a logical clock, so interleavings are reproducible and both backends agree. On the CLI, `noeta run` uses a real (tokio) executor and real OS-thread isolates. See [Concurrency Internals](Concurrency-Internals) for the "simulate deterministically, deploy real" design.

A streaming body is deterministic under the sandbox too: the responder decodes a scripted body that is a pure function of the request, so a reading loop terminates in-oracle and both backends observe the identical frames.
