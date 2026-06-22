# Slice M2.3 — Async-first IO runtime foundation (per-isolate tokio)

Status: todo

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** M2.1 (the `Host` boundary). **Determinism posture:** conformance keeps running `SandboxHost`, which resolves synchronously, so **async never enters the differential**. The async runtime exists only behind `RealHost` for `lang run`/REPL/server.

## Goal
Stand up a per-isolate tokio runtime and make the host IO **internals** async-capable, so that real IO (M2.4) and the eventual `async`/`await` surface are built on an async foundation from day one — **without any new surface syntax** in this slice.

## Why "async-first internals" now
Architecture §7 makes the runtime an isolate model: *"Each unit of concurrency gets its own heap … Intra-isolate async (`async`/`await` over a real scheduler) for I/O-bound concurrency."* §7.1 puts `async`/`await` over the tokio scheduler. If real disk/network IO (M2.4 and later the server) were written synchronously and retrofitted to async later, that retrofit would be a rewrite. Building the IO driver async **now**, while the surface stays synchronous-looking (the VM blocks on the leaf future at the call boundary), means the later `await` surface pass is *additive*, not a teardown. This is the explicit user-chosen scope: **internals/foundation only, no `async fn`/`await`/`concurrent { }`/`TaskScope` surface** (those are a separate later M2 pass, architecture §7.1–§7.2).

## Architecture (decided)
- **New crate `lang-runtime`** (depends on `lang-stdlib`, impls its `Host` trait; back-edge-free): a `tokio` `current_thread` runtime owned per isolate (matching the shared-nothing isolate model, §7), plus an async IO driver.
- **`RealHost`** lives here: its `fs`/`env`/IO ops are `async fn`s driven on the runtime. At the VM call boundary the runtime **blocks on the leaf future** (`block_on` at the edge) and returns a plain value — so the VM/`RunResult` contract is unchanged and no opcode learns about futures yet.
- The CLI/REPL/server construct `RealHost` + the runtime; conformance still constructs `SandboxHost` (sync, deterministic). Same `Host` surface, two impls — the M2.1 split pays off here.

## Scope
- In:
  - `lang-runtime` crate: per-isolate `current_thread` tokio runtime; async IO driver; `RealHost` implementing the `Host` trait with async internals, blocked-on at the boundary.
  - Wire `lang-cli` (and REPL) to run programs on `RealHost`/the runtime instead of `SandboxHost`.
- Out: **all surface syntax** — `async fn`, `await`, `concurrent { }`, `TaskScope`, `spawn`, channels (a later M2 pass per §7.1–§7.2); multi-isolate parallelism / inter-isolate channels (§7); the HTTP/WS server (§9.5, later M2); cancellation semantics.

## Checklist (vertical slice)
- [ ] Grammar / AST: none — explicitly no surface this slice.
- [ ] Checker rule: none.
- [ ] Bytecode: none — no opcode becomes async; the runtime blocks at the leaf.
- [ ] VM op: none changed; the `Host` IO calls now route to async impls behind `RealHost` (CLI path only). Tree-walker on `RealHost` mirrors via the same blocking boundary.
- [ ] Conformance cases: none new — conformance runs `SandboxHost`, so the differential is untouched (the *proof* that async stayed out of the oracle is that coverage/divergence are unchanged). A CLI integration test may exercise the `RealHost` path outside the differential.
- [ ] Snapshots: none.

## Definition of done
- `lang run` executes programs on the per-isolate tokio runtime via `RealHost`; existing behavior is observably identical to the sandbox for IO-free programs.
- `lang test --differential` is **unchanged** (still `SandboxHost`, 0 skipped, zero divergence) — async added no oracle risk.
- `lang-runtime` is `unsafe`-free; the DAG stays back-edge-free (`lang-runtime → lang-stdlib`, both backends + CLI depend inward).
- fmt/clippy clean.

## Notes / traps
- **No `async` leaks into the surface or the differential.** If a conformance case would behave differently sync-vs-async, the boundary blocking is wrong.
- `current_thread` (not multi-thread) per isolate is deliberate — it matches the shared-nothing, non-atomic-refcount isolate model (§7); a multi-thread runtime would invite the `Arc<Mutex>` data races §7 explicitly keeps out of userland.
- This slice's value is entirely in *not having to rewrite IO later*. Keep `RealHost`'s sync-looking boundary thin so M2.4 (real disk + streaming) and the future server slice extend it rather than fight it.
