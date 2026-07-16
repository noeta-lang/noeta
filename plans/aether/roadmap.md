# noeta-aether — first-party web framework (initiative roadmap)

**Goal:** a Laravel-grade, first-party web framework written in **pure Noeta**, living in the
`para` namespace as `para.aether`, composing the existing LiveView + reactivity + `@html` stack.
Building it is a deliberate **language stress test**: wherever a wall is hit, we extend the
language (build it right, not around).

Branch/worktree: `noeta-aether` (off main `cc3c55a6`).

## Product surface (target DX)

- **Controllers**: classes with attributed methods as routes — `#[Get("/users")]`, `#[Post("/users")]`.
- **Autodiscovery** of routes via reflection (`attributes_of::<Get>()` → `target = "Ctrl.method"`),
  dispatched with `invoke(instance, method, args)`.
- **Middleware** pipeline (before/after, short-circuit).
- **Dependency injection**: handler params materialized by type — `Request`, a typed request body
  (`CreateUser` decoded from JSON), or a bound ORM resource (`User` from `/users/{id}`).
- **Background task scope** (top-level) + **scheduler** (`every(...)`) — fire-and-forget from a handler.
- **Config registry** + **service providers** for modular apps.
- **Rendering** via `para.html` `@html{…}` / LiveView diff-push.

## What already exists (compose, don't rebuild)

- LiveView + `signal/computed/effect` + `@html` tier (`packages/para`, `std.reactive`).
- HTTP server (`std.http.server` `serve`, `fetch`, websockets, graceful shutdown, `--parallel`).
- Attributes on methods + `attributes_of::<T>()` + `invoke(recv, name, args)` reflection.
- Typed JSON decode `json.parse::<T>(s)` (concrete T only) + `@derive(Serialize<Json>)`.
- `std.cell` (reference-semantics shared box) for the config/service registry.

## Language gaps — the arcs the stress test forces

| # | Gap | Verdict |
|---|-----|---------|
| L1 | User-defined traits (closed built-in set today) | **BUILD (foundational, first).** Typed Controller/Middleware/ServiceProvider/Resource contracts + `dyn Trait`. |
| L2 | Typed deserialization for DI | **BUILD.** `@derive(Deserialize<Json>)` → `from_json` (mirror of Serialize) + **parameter-type reflection** so the router injects by declared param type. |
| L3 | Background tasks / scheduler alongside `serve` | **NO RUNTIME CHANGE (re-verified).** The serve loop already calls `ctx.advance_tasks()` each round (`serve.rs:650`), so it DRIVES sibling `spawn`ed tasks. `concurrent { spawn worker(); spawn scheduler(); serve(port, handler) }` runs server + background scope + scheduler on one heap. Fire-and-forget = a handler enqueues onto an in-heap `channel` the worker drains (PROVEN: jobs processed). Scheduler = a `sleep`-loop task (PROVEN: ticks). My earlier "starvation" spike was buffered stdout lost to a `timeout` kill — the user's original instinct (top-level async can host background tasks) was right. L3 = pure-Noeta framework API only. |
| L4 | Request ergonomics (header/query enumeration, cookies, forms) | Small `std.http` additions, as needed. |
| L5 | DB driver / real ORM persistence | **LAST.** Separate native `para/db` package. Aether ships a Resource/Repository seam + in-memory driver first. |

### L3 spike result (2026-07-15)
- The module top level is async only w.r.t. `.await` (`check lib.rs:2172`); `spawn` is gated on
  `concurrent_depth != 0` (`lib.rs:4847`), so a bare top-level `spawn` is E0041 — but a top-level
  `async fn run()` opening a `concurrent {}` nursery is fine. **A worker task runs correctly inside
  such a nursery (control spike passed).**
- **BUT `server.serve(port, handler)` returns `Unit`** (`serve.rs:64`) — a blocking native call
  running its own accept loop inline. A worker spawned beside it never gets polled (starves), and
  `spawn server.serve(...)` is rejected (`spawn` needs a `Future`, serve is `void`). So serve does
  **not** compose as a nursery sibling today.
- **CORRECTED (re-verified):** No async serve needed. The blocking `serve` loop calls
  `ctx.advance_tasks()` each round (`serve.rs:650`) → it drives sibling `spawn`ed tasks. So
  `concurrent { spawn scheduler(); spawn worker(); serve(port, handler) }` already runs server +
  background scope + scheduler on one shared heap. The original spike's "starvation" was buffered
  stdout lost to a `timeout` kill, not real starvation. Aether wraps this pattern as
  `background(job)` (enqueue to an in-heap channel a worker drains) / `every(ms, fn)` (a sleep-loop
  task) — pure Noeta, no runtime change.

## Sequencing

0. ~~**Spike** — `serve()` inside a nursery.~~ DONE: needs async serve (see L3).
1. **L1 user-defined traits** (compiler; own commits). See `plans/aether/traits.md`.
2. **L2 DI** — `Deserialize<Json>` derive + parameter-type reflection.
2b. **L3 async serve** — `serve` variant returning `Future<Unit>` (small, `serve.rs`).
3. **Aether vertical slice** — one controller (`#[Get]`+`#[Post]`), one middleware, typed-body inject,
   a fire-and-forget background job, a config value; renders via `@html`. Proves every seam.
4. **Broaden** — full middleware, service providers, scheduler, route-model binding (in-memory repo).
5. **para/db** (native) — real persistence, last.

## Constraints / coordination

- `para` is an **abstract namespace family, not a package**. `para.aether` is its **own package** at
  `para/aether`, sibling to `para/html` and `para/p2p`. Another agent is establishing that `para/`
  package structure. The language arcs (steps 1–2) touch **only compiler crates** — no package files —
  so the para rework settles before aether's `.noe` modules land in `para/aether`. Coordinate on the
  exact manifest/dir convention with that rework before scaffolding the package (step 3).
- Commit as you go (each green slice). Never push without authorization.
- Every language change validated under the differential/session-parity oracle.
