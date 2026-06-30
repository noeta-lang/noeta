# Coroutines — lazy iterators → generators → async/await (one substrate, three tracks)

**Status: NOT STARTED — design plan, for sign-off.** Branch suggestion `coroutines`. Standard commit
trailers. This is a *new track*. Provenance: a 2026-06-30 design discussion (generators as a
performance feature; "how do other languages do it; Rust nightly chose coroutines"). The conclusion
was that generators and `async`/`await` share one compile-time machine and should be **designed
together but shipped as separate tracks** with very different cost and determinism profiles.

## Why this exists

Today the language has only **eager** iteration:
- `for x in xs` *materializes* its source into a `Vec` up front (`lang_eval::Interpreter::iter_elements`):
  a list/set in canonical order, a map's values in key order, or a user object's `Iterable` via an
  **`iter()` method that returns a whole list**.
- `map` / `filter` / `sum` are eager prelude builtins — each `expect_list`s its argument and builds a
  brand-new list, so `xs.map(f).filter(g)` allocates an intermediate list per stage.
- There is no `yield`, no lazy `next()`, no `async`/`await`. M2 built the tokio-per-isolate IO
  internals but **deliberately deferred the async surface** ("a later M2 pass", see
  [[m2-host-io-cluster]]).

The win is **pull-based laziness**: fused pipelines that allocate nothing per stage, streaming over
sources too big to materialize, and infinite/computed sequences. It is the in-memory twin of what
**P-LAZY** just did for files (`plans/perf/p-lazy-fs-open.md`) — produce on demand, not all up front.
Because generators are **pull-driven** (the consumer calls `next()`), they need **no scheduler** and
stay **deterministic**, so they live inside the differential oracle. Async does not — that is the
fault line this plan is built around.

## The deciding constraint: two backends + the differential oracle

Coroutines must *suspend and resume*. There are two families, and the architecture — not taste —
picks one:

- **Stackful** (a coroutine owns a real stack; suspend = swap stacks; Lua coroutines, Ruby Fibers,
  Go goroutines). The **VM** could do this fairly cleanly (a suspended coroutine is a saved frame:
  IP + register window, stored as a heap value, à la Lua/CPython frame objects). But the
  **tree-walker has no frame to save** — it rides the Rust call stack; suspending it means a separate
  OS stack per coroutine or a CPS rewrite. That yields *two different* suspension implementations
  that must be proven observably identical — exactly what the differential oracle exists to prevent.
- **Stackless** (the compiler rewrites a coroutine into a **state machine**: locals → state fields,
  each suspend point → a state, `next`/`poll` switches on the state; Rust nightly `gen`/coroutines,
  Python, JS, C#, Kotlin, C++20). Here **neither backend suspends at runtime** — the coroutine is an
  ordinary object with a `next()` method. Do the transform in the **shared IR** (`lang-ir`, where
  `for (a,b) in …` destructuring and the `json.parse::<T>` recipe are already desugared) and both
  backends run it identically *by construction* — the same guarantee every other shared lowering
  gives.

**So the architecture forces stackless, transformed in the shared lowering.** This is not a
compromise: it is also the most perf-friendly (no per-coroutine stack; the only allocation is the
state object) and where the mainstream landed. Rust chose stackless for zero-cost / no-runtime /
embeddability; we are pushed there by the oracle, for a different reason but to the same place.

### How other languages approach it (survey, for the record)

| Language | Model | Notes |
|---|---|---|
| **Rust** (stable) | none — hand-written `Iterator::next()` structs | survived years on this; laziness via ordinary adapter structs |
| **Rust** (nightly `gen`/coroutines) | **stackless** state machine | *same machinery* as `async`; coroutine is the primitive, `gen`/`async` are sugar |
| **Python / JS / C# / Kotlin / C++20** | **stackless** | `yield` + `async`/`await` unified on one transform; the wart is **function coloring** |
| **Lua / Ruby Fibers / Go** | **stackful** | suspend from any depth ("colorless"), but needs runtime stack management; harder to make deterministic/portable (Go adds a scheduler → green threads) |
| **Zig** | shipped then **removed** async | a caution that this is easy to get subtly wrong |

The axis to remember: **stackless = colored + cheap + compile-time transform; stackful = colorless +
flexible + runtime stacks.** Our oracle wants stackless.

## One substrate, three tracks

| Track | Delivers | Determinism / oracle | Cost |
|---|---|---|---|
| **I — lazy iterator protocol** | `Iterator` (`next() -> ?T`) + `Iterable` (`iter()`); `for` drives it; lazy `map`/`filter`/`take`/`zip`/`enumerate` adapters | deterministic → **in-oracle** | small–moderate (mostly front-end) |
| **G — generators (`yield`)** | `yield` sugar → the stackless state machine on the shared substrate; a generator *is* an `Iterator` | deterministic → **in-oracle** | moderate (the transform + liveness) |
| **A — async/await** | `await`, an awaitable/`Future` type, an executor over the per-isolate tokio | non-deterministic IO → **out-of-oracle** | milestone (runtime + determinism story) |

Sequencing: **I → G → A**. I is independently valuable and ships alone. G needs the substrate. A is a
later milestone — but **planned now so the substrate is async-ready** and we don't build the transform
twice (Rust/Python/JS/C# all build generators and async on one machine).

## The shared substrate (the build-once core)

A **stackless state-machine transform** in `lang-ir`, plus one runtime fact that makes it cheap:

- **No new runtime value kind, and no runtime suspension.** The transform's output is an ordinary
  object with a `next()` method and hidden state fields (a state discriminant + the locals live
  across suspend points). The VM runs it through its normal object/method path; the tree-walker
  through its normal `call_method`. Neither backend gains a "suspend" opcode. This is what keeps the
  two in lockstep.
- **An iterator/generator is a reference type (a `class`), not a value `struct`.** Calling `next()`
  *advances* it, and that advance must be visible to aliases — interior, shared mutation. The object
  model already has exactly this split (value `struct` vs reference `class` with identity), and
  `FileHandle` is already the precedent for a mutable reference-semantic value
  ([[m2-host-io-cluster]]). So iterators reuse `class` reference semantics — no carve-out.
- **Precise RC fits.** A suspended coroutine's state object owns its live locals under the existing
  refcount backbone; pull-based iteration needs no scheduler and introduces no cycles in the common
  case. Determinism is preserved (the consumer drives; no wall-clock, no ordering nondeterminism).
- **Rust-style layering:** the state machine is the one primitive; **Track G `yield` and Track A
  `await` are both sugar that lower to it**, differing only in the *resume driver* (a `next()` caller
  vs an executor poll) and the *suspend value* (a yielded element vs an awaited future).

The hard compiler content (shared by G and A):
- **Liveness across suspend points** — which locals must survive a `yield`/`await` and become state
  fields vs which are dead and can stay temporaries.
- **Restricted control flow through a suspend** — the language already has restricted control-flow
  heads (object-model slice 7b); the transform defines what control structures may straddle a
  suspend point.
- **Coloring** — a suspend may appear only directly in the coroutine body, **not inside a closure it
  passes to a builtin** (you cannot `yield` from within a `map` callback). This is the standard
  stackless limitation; name it in the surface docs from day one.

## Track I — lazy iterator protocol (ships first)

The foundation, and **mostly front-end** (no suspension machinery at all — adapters are ordinary
reference objects).

- **Protocol.** `Iterable` = "has `iter() -> Iterator`"; `Iterator` = "has `next() -> ?T`" (the
  existing `?T` optional + `Option` make the signature fall out: `some(x)` = an element, `none` = end).
  A list/set/map is `Iterable`; this *upgrades* the existing eager `iter()→list` hook to a streaming
  `iter()→Iterator` while keeping the list itself a plain value.
- **`for` desugar** (in the shared lowering, replacing `iter_elements`' eager materialization):
  ```
  for x in src { body }
  ⇓
  it = src.iter()
  loop {
      match it.next() {
          some(x) => { body }
          none    => break
      }
  }
  ```
  A list's `iter()` returns a tiny cursor iterator (index + backing) — so `for` over a list is still
  O(1) memory and the existing tuple-destructuring `for (a,b) in …` rides along unchanged.
- **Lazy adapters** — `map`/`filter`/`take`/`drop`/`zip`/`enumerate`/`chain` as `Iterator` objects
  wrapping a source iterator + (for map/filter) a closure, pulling one element per `next()`. Fused: no
  intermediate list. A terminal `collect()` (or `to_list()`) materializes when the consumer wants a
  list back.
- **Decision — lazy-by-default vs explicit `.iter()`.** Recommend **explicit, Rust-style**: keep the
  current eager `xs.map(f)` (value-semantic, returns a list) and add the lazy surface behind
  `xs.iter().map(f).filter(g).collect()`. Rationale: preserves value semantics and least surprise;
  laziness is opt-in where it pays. (Alternative — make `map`/`filter` lazy by default — risks
  surprising aliasing/effect-ordering and is a breaking change.) **Settle with the user.**
- **Determinism:** fully in-oracle; both backends agree by construction (shared desugar + ordinary
  objects).

## Track G — generators (`yield`)

`yield` as sugar onto the substrate.

- A function containing `yield` lowers (in `lang-ir`) to a `class` implementing `Iterator`: locals
  live across a `yield` become fields, the body becomes a `next()` state machine, each `yield e`
  returns `some(e)` and parks at the next state, falling off the end returns `none`.
- Inherits the **coloring** limit (no `yield` inside a closure passed to `map`) and the
  restricted-control-flow-across-suspend rules from the substrate.
- Determinism: in-oracle, same as Track I — a generator is just an `Iterator` whose `next()` happens
  to be a generated state machine.

## Track A — async/await (planned now, built later)

Rides the **same** state machine; the differences are all about the *runtime*, and they are why this
is a separate milestone rather than "generators with a different keyword."

Decision points to settle before A is built (none block I/G):
- **Determinism in testing — the big one.** Async IO/timing is non-deterministic, so async **cannot
  sit in the differential oracle the usual way.** Two routes: (a) async is simply *outside* the
  oracle, integration-tested only — mirroring the existing `RealHost` split (real IO is CLI-only,
  never differential-tested); or (b) a **deterministic sandbox scheduler** that makes async programs
  reproducible enough to differential-test. **Recommend (a) first**, with (b) as a later hardening
  step if async coverage needs the oracle.
- **`await` ↔ the per-isolate tokio.** M2 built the tokio `current_thread` runtime per isolate
  precisely so the async surface would be additive ("`block_on` at the leaf today → `await` later").
  A pins down the awaitable/`Future` type and how `await` maps onto that runtime.
- **Executor model & the `Future`/awaitable type** — what a suspended async fn returns, who polls it,
  how IO completion wakes it.
- **Unify or split coloring** — is there one "colored function" concept spanning `gen` and `async`
  (Rust's coroutine substrate), or two distinct surfaces sharing only the transform? Recommend the
  unified substrate internally even if the *surfaces* stay distinct.
- **Structured concurrency / cancellation** — the deferred `TaskScope`/spawn/cancel surface
  ([[m2-host-io-cluster]] names it) belongs to A, layered after bare `async`/`await`.

## Consolidated decision points (for the user)

1. **Lazy-by-default vs explicit `.iter()`** (recommend explicit/Rust-style; preserves value
   semantics). ← gates Track I surface.
2. **Protocol surface** — trait names (`Iterable`/`Iterator`), method names (`iter`/`next`), terminal
   (`collect` vs `to_list`).
3. **`next()` return** — `?T` (recommend; reuses `Option`).
4. **Generator coloring + restricted control flow across a suspend** — confirm the stackless limits
   are acceptable.
5. **Async oracle treatment** — out-of-oracle first (recommend) vs deterministic scheduler.
6. **Async runtime** — `Future`/executor model; unify coroutine substrate yes/no.
7. **New diagnostics** — next free code is **E0039** (e.g. *not iterable*, *`next` must return `?T`*,
   *`yield` outside a generator / inside a closure*).

## Sequencing & gating

- **Track I** — independently valuable, in-oracle, mostly front-end; ship first.
- **Track G** — after the shared substrate exists; in-oracle.
- **Track A** — a later milestone; **its decision points are recorded now so the substrate is
  designed async-ready** (build the state machine once). Gated on the async-runtime decisions above
  and coupled to the deferred concurrency surface.

## Verification (every slice)

- **I and G:** `cargo run -q -p lang-conformance` (corpus green), `--differential` (matched / **0
  skipped** / backends agree), `--check-leaks` (residency 0 both). New conformance cases: fused
  pipeline (no intermediate list), infinite generator + `take`, early `break`, tuple-destructuring
  `for` over a lazy source, the coloring error.
- **A:** integration-tested outside the corpus (like `RealHost`), *not* in the differential, unless
  the deterministic-scheduler route is taken.
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt --all --check` clean;
  **miri** only when `lang-value` is touched (the iterator object is an ordinary `class`, so likely
  it is not).
- **Bench the perf claim** (per `plans/perf` discipline): allocation count / time for
  `xs.iter().map().filter().take(k)` vs the eager `xs.map().filter()` over large `xs` — the fused
  pipeline should allocate O(1) intermediate vs O(stages·n). Record numbers in the slice doc.

## Relationship to other tracks

- **P-LAZY** ([[m2-host-io-cluster]]) is the IO precedent for the same pull-based-laziness shape; a
  lazy file-line `Iterator` falls out of Track I naturally (`fs.open(path,"r").iter()` over lines).
- **Object model** — iterators are reference `class`es; reuses the value/reference split, no new kind.
- **Async** is coupled to the deferred M2 concurrency surface; this doc is where that coupling is
  recorded so the substrate is built once.
