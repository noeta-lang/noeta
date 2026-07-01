# Track A — async / await

Parent: `plans/coroutines/README.md` (the substrate + the executor decision are settled there);
architecture `docs/resources/01-architecture.md` §7 (isolates) and §7.1–§7.2 (async + structured
concurrency). **Status: PLANNED — for build** (branch `main`, per repo convention). Builds directly on
the completed Track G (the stackless state-machine lowering in `lang-ir/lower.rs`) and on M2's
per-isolate tokio runtime (`lang-runtime`, `RealHost`).

## Why this rides the generator substrate

An `await` point is the **same stackless suspend** as a `yield`, with two differences (parent doc):
the **resume driver** is an *executor poll* instead of a `next()` caller, and the **suspend value** is
a *future being polled* instead of an element being yielded. Concretely, `x = f().await` lowers almost
exactly like G.4's `for`-head: hoist the awaited future into a cell, then a **poll-state** that either
**advances** (the future is `Ready(v)` → bind `v`, go to the next state) or **self-loops returning
Pending** (the future is `Pending` → `$state = poll_state; return pending`, re-polled on re-entry). So
the CFG flattener, cell hoisting (liveness), and dispatch loop from Track G are reused wholesale; the
new pieces are the `.await` surface, the poll protocol, the `Future` value, and the executor.

## Settled decisions (with the user, 2026-07-01)

1. **`async fn f(...): T` — `async` keyword, inner return type.** A body `return`s a `T`; the call
   `f(...)` has type `Future<T>`; `expr.await` unwraps `Future<T>` → `T`. Unlike generators (no `gen`
   keyword — `yield` marks them), async takes a keyword: coloring is visible at the declaration and the
   return-value semantics are clean. Matches Rust/C#/Kotlin/TS.
2. **Postfix `.await`.** Chains cleanly with `?` and method calls (`fetch(url).await?.text().await?`),
   as architecture §7.1 already commits to (`query(...).await?`). `await` is a keyword; `.await` is a
   postfix operator → `Expr::Await`.
3. **Implicit async top-level, no `block_on`.** The module top-level body compiles as an async root
   task **only if it contains a top-level `.await`** (one not inside a nested `fn`); the executor drives
   it to completion. A program with **no top-level `await` is byte-identical to today** (gated exactly
   like generator detection), so the corpus + differential are untouched by construction. This makes
   the root task the main isolate's entry task (Rust `#[tokio::main]` shape) and removes any need for a
   `block_on` bridge — which on a single-threaded executor could only ever be a root-only escape hatch
   anyway. Coloring still governs *nested* sync `fn`s (they can't `await`) — inherent to stackless.
4. **Full in-oracle async.** Bare `async`/`await` + a deterministic sandbox executor + suspending leaf
   futures (`ready`, logical-time `sleep`) + structured concurrency, all in the differential oracle.
   Real tokio IO leaves are wired last, CLI-only, out-of-oracle.
5. **Concurrency is structured, not loose** (architecture §7.1/§7.2). One primitive — **`TaskScope`** —
   with **`concurrent { }`** as its block-lifetime sugar; **`spawn` is a compile error without an
   owning scope** (a `concurrent` block or an async-fn body), which makes orphaned tasks impossible by
   construction. Not bare spawn/join. `all`/`race`/bounded-parallel `map` are library functions over
   this (later, not language constructs).

## The executor — an injected capability (mirrors `Host`)

The resume driver is an **injected capability**, exactly like `Host` (`lang-stdlib/src/host.rs`):
an object-safe `Executor`/scheduler trait, `Box<dyn Executor>` held per interpreter, with two impls —

- **`SandboxExecutor`** (deterministic: single-threaded, logical time, deterministic ready-queue
  ordering) — the one the conformance corpus + `--differential` always run → **in-oracle**.
- **`RealExecutor`** (the per-isolate tokio `current_thread` runtime M2.3 already built) — CLI/REPL/
  server only, **out-of-oracle** (integration-tested outside the corpus, so `skipped` stays 0).

This is the same `SandboxHost`/`RealHost` split extended to *scheduling* — the FoundationDB
"simulate deterministically, deploy real" model. The **scheduling logic is shared** (a deterministic
scheduler struct both backends drive with a backend-supplied *poll callback*, exactly as
`iter_next_apply` takes a backend `apply` closure), so the two backends agree on ready-queue order and
logical time **by construction**. `current_thread` (single-threaded) is deliberate and *required* by
the isolate model (§7): non-atomic refcounts + shared-nothing per isolate; a multi-thread runtime would
invite the `Arc<Mutex>` races §7 keeps out of userland. Stackless tasks are heap objects in the
isolate's own heap under the existing refcount backbone — so async fits the isolate model *better* than
stackful would (no per-task OS stack, no locks, one cooperative scheduler per isolate).

## The `Future` value + poll protocol

A `Future<T>` is a step-closure state machine (like `IterState::Gen`) wrapped in a **new runtime value
kind** (`Payload::Future` in lang-value + the mirrored `Value::Future` in lang-eval — the same
two-backend mirror discipline as `IterState`). Calling an `async fn` **does not run the body**; it
produces the `Future` (state cells + step closure). The step's resume arg is unit/ignored (as in Gen —
an awaited value flows through the poll op, not the resume arg). **Poll op** `Op::PollFuture` (both
backends): poll a `Future` → `Ready(v)` / `Pending`. The internal poll result reuses `Option`
(`some(v)` = ready, `none` = pending) to reuse `option_take` and the existing machinery — it is
compiler-synthesized and never user-visible (the polarity note lives in the lowering). The executor
drives the root future: poll → on `Pending`, advance the scheduler (run a ready timer/task) and
re-poll; on `Ready(v)`, done; if `Pending` with nothing to advance → a deterministic deadlock error.

## Sub-slices (each its own green, in-oracle commit)

- **A.0 — surface + typing + executor seam + `Future` runtime (gated, not executable).** Mirrors G.1a.
  - Lexer: `async`, `await` keywords. Parser: `async fn` (`FnDecl.is_async`); postfix `.await` →
    `Expr::Await`. AST nodes (+ pretty).
  - Type: `Future<T>` (writable annotation in `PRELUDE_TYPES`). An `async fn`'s call type is
    `Future<T>` where `T` is the declared inner return; `e.await` where `e : Future<T>` has type `T`.
  - Checker (coloring, **E0040**): `.await` only inside an `async fn` **or** at async top-level; a bare
    `.await` in a sync `fn` → E0040; the yield-style reset at closure boundaries so `.await` inside a
    closure passed to a builtin → E0040. `?` typing through `.await` (`Result` unwrap) — spec now,
    enforce as the surface lands.
  - **Scope decision (revised at build): A.0 is FRONT-END ONLY** — the `Future` runtime value
    (`Payload::Future`/`Op::PollFuture`) and the `Executor`/`SandboxExecutor` seam moved to **A.1**,
    where they are actually driven end-to-end (and miri runs on the real usage) rather than added as
    dead code now. Rationale: the A.0 gate blocks every async program at *check* time, so no runtime
    code is reachable in A.0 — unlike G.1a (which pre-added `IterState::Gen` because iterators were
    brand-new infra), async reuses the proven state-machine substrate, so the value kind is best
    introduced with its driver. A.0 therefore touches no `lang-value` → miri not required this slice.
  - **Interim gate (`E0040`):** every `async fn` and an implicitly-async top level (a top-level body
    with a `.await`) emit a clean *"not yet executable (Track A.1)"* — the program type-checks but
    cannot run yet, so no async program reaches lowering (lowering-is-total invariant preserved). The
    `Expr::Await` lowering arm returns `Unsupported` as a belt-and-braces backstop.
  - Conformance (`tests/conformance/async/`): error cases only — the not-yet-executable gate on a
    well-formed async fn (which also proves `call → Future<T>`, `.await → T`, and `.await?` chaining
    type correctly), `.await` outside async (coloring), `.await` in a closure (coloring), `.await` on
    a non-future, and top-level `.await`. **DONE** (`3aa0e13`-follow-on): conformance 355 / differential
    0-skipped / leaks 0 both / clippy+fmt+workspace clean.
- **A.1 — minimal executable *sequential* async/await. ✅ DONE.** (revised split, at build). Rationale: a full
  state machine with **no suspending leaf** (none exist until A.2) would leave the entire `Pending` path
  dead and untested — so A.1 ships the smallest end-to-end-*testable* async instead, and A.2 adds the
  state machine together with the leaf that exercises it. A.1 delivers: **`Payload::Future`/
  `Value::Future`** (both backends, miri) wrapping a **lazy thunk** (Rust-style: the body runs on await,
  not at the call); `Op::MakeFuture` (async fn call → `Future<T>`, body deferred into the thunk) +
  `Op::RunFuture` (run a future's thunk to completion, what `.await` lowers to); an
  `Executor`/`SandboxExecutor` seam is *deferred to A.2* (A.1's run-to-completion is a direct thunk
  call — trivial, nothing suspends). Implicit async top-level: the top level runs directly and its
  top-level `.await`s lower to `RunFuture` (no root-future wrapping needed until A.2 introduces
  suspension); the checker sets `current_async` for the top level via `block_has_await`. Removes the
  A.0 gate. Both backends (the tree-walker mirrors `Payload::Future` as `Value::Future`; a
  non-future operand passes through so evaluation stays total for the uncheck­ed property test).
  Fully tested (no dead path: nothing suspends → no `Pending`). Conformance `tests/conformance/async/`:
  `sequential` (awaits across locals, repeated awaits, nested async calls), `top_level_await`,
  `try_through_await` (`?`-through-`.await`), plus the three A.0 error cases retained. Conformance 356 /
  differential 347 matched 0-skipped / leaks 0 both / miri clean / clippy+fmt+workspace clean.
- **A.2 — first suspending leaf (`sleep`/timer) + executor. NO state machine (regrouped with user
  2026-07-01).** Adds the `Executor`/`SandboxExecutor` injected seam (a deterministic logical clock +
  a timer set) and a `sleep(ms)` leaf future that returns `Pending` until the clock reaches its
  deadline. A.1's `RunFuture` (drive-to-completion, used by every `.await`) is upgraded to **advance the
  clock on `Pending` and re-poll** — so a `sleep` awaited anywhere completes by advancing logical time.
  `Pending` is genuinely exercised (the timer pends at the leaf; the executor advances; a pending with
  no timer is a deterministic deadlock error). In-oracle.
  - **Why no state machine here (the regrouping):** in *single-task* async — all we have until A.3 —
    every `.await` must complete before execution proceeds (no sibling task to yield to), so "drive the
    awaited future to completion inline, advancing the clock on `Pending`" is **observationally
    identical** to "suspend the async fn as a state machine and let the executor re-poll it." The state
    machine's one distinctive behavior — suspending a task so a **sibling** runs — is unobservable
    without A.3's `concurrent`/`spawn`. Building it in A.2 would leave that path dead/untested (the exact
    anti-pattern the A.1/A.2 split avoided), so the state machine moves to A.3, where it is finally
    testable. A.1's tests still pass unchanged (single-task drive-to-completion regardless of mechanism).
- **A.3 settled decisions (with the user, 2026-07-01):** **explicit-only** structured concurrency —
  `spawn` is legal *only* inside a `concurrent { }` block (an orphan `spawn` is **E0041**); there is no
  bare top-level `spawn` (write `concurrent { }`, which may itself sit at the top level). `concurrent`
  is what guarantees nothing dangles: the block cannot be exited until every task spawned in it has
  finished (joined at `}`), and a child's failure propagates at that boundary. `concurrent`/`spawn` are
  async-only (a sync fn has no suspend machinery). `spawn e` takes a `Future<T>` (an `async fn` call)
  and returns a **handle that is itself a `Future<T>`** — `h.await` yields the result; a richer `Task`
  type (cancel/status) is deferred. Execution model: the `concurrent` block body runs inline
  (`ScopeBegin`; body; `ScopeEnd`); `spawn` registers a task in the current scope (lazy — not run until
  first drive); a `.await` **inside the block body** drives the scope (round-robin poll of sibling
  tasks + the target, advancing the logical clock) so siblings interleave, while a spawned task's *own*
  `.await`s are the A.3a poll-suspends (it yields to the scheduler); `}` joins all remaining tasks.
  Deferred: `all`/`race`/bounded `map`, cancellation beyond abandon-on-error, a nested `concurrent`
  interleaving with outer siblings (a nested scope runs to completion within its task). Built in two
  green commits — **A.3b.1** front-end (surface + typing + E0041, gated "not yet executable") then
  **A.3b.2** the cooperative scheduler runtime.
- **A.3 — structured concurrency + the async state machine — ✅ DONE** (A.3a `e0d408e` state machine;
  A.3b.1 `ffd74a2` surface/typing/E0041 gated; A.3b.2 `8d2a23b` cooperative scheduler — executes). See
  the settled-decisions block above and [[coroutines]] memory for the built shape.** The state machine is built here because concurrency is what makes it
  observable: the CFG state-machine lowering (a `lower_async` mirroring `lower_generator`: `.await` →
  hoisted future cell + poll-state that advances on Ready / self-loops returning `Pending`) + the
  `Op::PollFuture` single-poll op replace A.1's thunk so a task can suspend and yield to a sibling.
  `concurrent { }` block-lifetime scope; `spawn` legal only inside a scope (orphan `spawn` → a new
  diagnostic code); block joins at `}`; child errors propagate at the boundary; deterministic
  ready-queue ordering in `SandboxExecutor`. `TaskScope` value. **Decide:** does an implicitly-async
  root count as an owning scope for a top-level `spawn`? (settle here). `all`/`race` deferred to a
  library.
- **A.4 — real executor (tokio) + async IO leaves (CLI-only, out-of-oracle).** Wire `.await` to the
  per-isolate tokio `current_thread` runtime in `RealHost`; make fs IO **suspend** (`await`) instead of
  `block_on` at the leaf. `RealExecutor` behind the CLI, integration-tested like `RealHost`.
- **A.5 — finalize.** Coloring cleanup, `?`-through-`.await` + error/cancellation typing (§7.1), docs
  refresh (`resources/01,02`, this dir's README + memory), `plans/deferred.md` rows, mark Track A
  complete.

## Diagnostics

**E0040** (next free code — see `lang-diagnostics/src/lib.rs`, currently ending at E0039) is the async
coloring code (`AsyncMisuse`: `.await` outside an async context / inside a closure; bad async return).
A.3's orphan-`spawn` check takes the next code after that. Append-only to the enum, `ALL`, and `code()`.

## Deferred to later passes (recorded so they are not lost)

- **Inter-isolate parallelism / channels** (§7 CPU-bound story) — Track A is intra-isolate async only.
- **App-lifetime `TaskScope` via DI, workers, durable queues, schedulers** (§7.2) — framework/
  first-party-extension patterns over the `TaskScope` primitive, not language constructs.
- **Cancellation semantics** beyond the structured-scope basics; `all`/`race`/bounded-parallel `map`
  as library functions.
- **Persistent runtime / bundled HTTP-WS server / signals** (roadmap M2 later clusters).

## Verification (every sub-slice, A.0–A.3)

`cargo run -q -p lang-conformance` (corpus green) + `--differential` (0 skipped, backends agree) +
`--check-leaks` (residency 0 both); `cargo test --workspace`; `cargo clippy --workspace --all-targets`;
`cargo fmt --all --check`; **miri** (`cargo +nightly miri test -p lang-value`) when `lang-value` is
touched (A.0 adds the `Future` value kind → miri runs). A.4 is integration-tested outside the corpus
(like `RealHost`), never in the differential. New conformance per slice as noted above.
