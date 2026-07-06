# Concurrency

The language has lazy iterators, generators, `async`/`await` with structured concurrency, and true-parallel isolates communicating over typed channels — all built on one stackless state-machine substrate (see [Concurrency Internals](Concurrency-Internals) for how). This page is the surface.

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

`.await` inside a closure is E0040 (function coloring), and a conditionally-evaluated mid-expression `.await` (e.g. the right side of `&&`) is E0040 — an unconditional one is hoisted automatically.

## Structured concurrency

A `concurrent { … }` scope runs tasks concurrently and joins them at the closing brace. Inside it:

- **`spawn expr()`** schedules a future as a task, yielding a handle you can `.await`.
- **`isolate f(args)`** runs in a fresh isolate (own heap, true parallelism); its arguments and result must be `Send` (see below).

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
| `race(list)` | Returns the first result; losers are cancelled cooperatively. |
| `map_bounded(items, n, f)` | Applies async `f` to each item with at most `n` in flight; results in item order. |

## Isolates and `Send`

An **isolate** is a shared-nothing unit of execution — its own heap, communicating only by message. `isolate f(args)` runs `f` on a fresh isolate with real parallelism.

Only `Send` values may cross an isolate boundary, and the **value/reference axis is the shareability axis**:

- **`Send`**: value types — primitives, `bytes`, tuples, structs of `Send` fields, enums, and `List`/`Map`/`Set`/`Option`/`Result` of `Send`.
- **`!Send`**: reference types (`class` — they have identity and shared mutation) and `dyn`.

Sending a `!Send` value across an isolate is E0042.

## Channels

A bounded, typed channel connects tasks or isolates:

```noeta
async fn produce(tx: Sender<int>): void {
    for i in 0..5 { tx.send(i).await }
    tx.close()
}

async fn consume(rx: Receiver<int>): int {
    mut total = 0
    mut running = true
    while running {
        (delta, keep) = match rx.recv().await {
            some(v) => (v, true),
            none    => (0, false),
        }
        total = total + delta
        running = keep
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
- `tx.close()` — marks the channel closed.
- `rx.recv().await` — `some(v)` while values remain, `none` once closed and drained.

## Determinism

In the sandbox executor (used for the differential oracle) time is a logical clock, so interleavings are reproducible and both backends agree. On the CLI, `noeta run` uses a real (tokio) executor and real OS-thread isolates. See [Concurrency Internals](Concurrency-Internals) for the "simulate deterministically, deploy real" design.
