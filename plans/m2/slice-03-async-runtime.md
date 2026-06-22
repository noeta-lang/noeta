# Slice M2.3 — Async-first IO runtime foundation (per-isolate tokio)

Status: **done**

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

## Outcome (done)

Landed the **`lang-runtime`** crate (`lang-runtime → lang-stdlib`, back-edge-free; `unsafe`-free) with **`RealHost`**: a per-isolate `tokio` `current_thread` runtime that drives real-disk file IO via `tokio::fs`, **blocked-on at the leaf** so the `Host` surface stays synchronous and no opcode/surface learns about futures. Real `env`/`args` read `std::env`; PRNG/clock stay deterministic (real time/entropy is a deliberate later choice, not a side effect of this slice).

**Host injection plumbing.** Both backends gained a host-injecting entry point next to the sandbox default: `VmBackend::run_module_with_host` and `TreeWalkBackend::run_with_host` (and `Interpreter::with_host`); the VM's `execute` now takes the host. The default paths (`run_module`/`Backend::run`) still build a fresh `SandboxHost`, so the conformance differential is byte-for-byte unchanged. `lang run` (`run_linked`) constructs `RealHost` and runs on it.

**Boundary adjustment from the original sketch (recorded).** The sketch deferred *all* real disk to M2.4, which would have left this slice's tokio runtime driving nothing real. To make the runtime earn its place, **flat** real-disk fs (`read`/`write`/`exists`/`remove`/`list`, paths relative to cwd) landed here — that is the concrete async IO the runtime drives. This required making the `Host` fs methods that touch storage **fallible** (`fs_write`/`fs_remove`/`fs_list` now return `Result`, `SandboxHost` always `Ok`); both backends' `call_fs` handle the `Result` (sandbox never errors, so the differential is unchanged). **M2.4 keeps** the genuinely new surface: streaming handles + a directory/path hierarchy model (richening the flat namespace).

**Verification:** `cargo test --workspace` **316 passed / 0 failed** — including two `lang-runtime` unit tests (real-disk round-trip in a temp dir; deterministic PRNG) and two CLI integration tests proving `lang run` reads the **real** environment (injected via the child's env) and writes to the **real** disk (a temp working dir), both *outside* the differential. `lang test --differential` **unchanged at 91 matched / 0 skipped / 100% / backends agree**; conformance 97 passed; M2.0 hot-path benches unchanged (~29.6/1.49/1.34 ms); fmt/clippy `--all-targets` clean; no `unsafe`.
