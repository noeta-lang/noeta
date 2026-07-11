# WASM target arc (P-WASM) — playground + edge

**Status: planned, not started.** Goal: make wasm a first-class deployment target driven by two
confirmed use cases (user, 2026-07-11): **an in-browser playground on noeta.dev** (the whole
toolchain — lex/parse/check/compile/run — client-side) and **edge deployments** (Noeta programs
running on wasmtime/Fastly/Spin-class runtimes, ultimately serving HTTP via `wasi:http`). This is
the M3 roadmap item "WASM target" (`plans/roadmap.md`, M3 row).

Provenance: design conversation 2026-07-11. Explicitly **out of scope for this arc** (confirmed):
wasm-threads-backed isolates (isolates degrade to cooperative tasks), the JIT tier under wasm,
p2p/local-first in the browser, and **direct wasm codegen** for Noeta functions (the L3-AOT analog)
— see *Out of scope* for the rationale on each.

## What exists today (verified against the repo, 2026-07-11)

- **`wasm32-wasip1` already checks clean, zero changes.** `cargo check --target wasm32-wasip1` over
  the full interpreter pipeline — `noeta-vm`, `noeta-stdlib`, `noeta-compiler`, `noeta-lexer`,
  `noeta-parser`, `noeta-loader`, `noeta-db`, `noeta-bundle`, `noeta-eval` — compiles with no
  errors on rustc 1.96. The architecture paid for this in advance: no tokio/Cranelift/threads on
  the default path, all effects behind the `Host` trait.
- **`wasm32-unknown-unknown` has exactly one blocker: `getrandom` via `bcrypt`.** The `bcrypt`
  crate's `alloc` feature unconditionally pulls `getrandom`, whose 0.3 backend is a
  `compile_error!` on `unknown-unknown` without explicit wiring. It is **dead code for us** —
  `crypto.rs` uses `bcrypt::hash_with_salt` only; the 16-byte salt arrives as an argument drawn
  from the Host `Entropy` capability (`crates/noeta-stdlib/src/crypto.rs:69-81`). Fix is build
  wiring, not code: `--cfg getrandom_backend="custom"` + a stub (or the `wasm_js` backend) in the
  browser crate.
- **No unconditional threads on the embedded path.** Off-thread JIT is behind the `jit` feature
  (default-off in `noeta-vm`); real-thread isolates are behind the runtime flag
  `parallel_isolates`, default `false` — isolates fall back to cooperative tasks on the
  single-threaded scheduler (`crates/noeta-vm/src/scheduler.rs:445`). The REPL/session path
  already runs this configuration.
- **`SandboxHost` is already a wasm host.** Every capability (fs/rng/clock/entropy/ids/network/
  env/os/telemetry) implemented deterministically with zero OS deps; `host.rs:188` even documents
  `"wasm32"`-style arch constants for it. `RealHost` (tokio/reqwest/p2panda) is CLI-only in
  `noeta-runtime` and never enters a wasm build.
- **The NaN-boxing core is width-agnostic.** `noeta-value/src/heap.rs` is the miri-sound
  exposed-provenance int↔pointer round-trip; 32-bit wasm pointers fit the codec. No mmap, no
  executable memory, no `target_os` cfgs outside `noeta-jit`/`noeta-runtime`/`noeta-cache`.
- **The bundling ladder carries over.** `.noeb` bundles (`noeta-bundle`, L1) are plain data;
  the per-ring DCE feature architecture (P-AOT L3.4, Axis B) applies directly to wasm binary size.
- **Cranelift cannot emit wasm** — it consumes it (wasmtime's backend). The L3 native-AOT path has
  **no** wasm equivalent; a compiled-bodies wasm target would be a brand-new emitter. Deferred
  (see *Out of scope*).

## Posture (inherited from the AOT/perf arcs)

- **New differential oracle: wasm-run ≡ native-run, byte-identical.** Every conformance program
  executed under wasmtime must produce a `RunResult` identical to the native VM (stdout/exit/
  diagnostics), `0 skipped`. `SandboxHost` determinism makes this well-defined; the WASI host gets
  the same treatment the RealHost got — never differential-tested, but the sandbox configuration
  of the *same wasm binary* is.
- Determinism unchanged; no wall-clock in output.
- Gates per slice: conformance, backend differential, `cargo test --workspace`, clippy, fmt, plus
  the new wasm-target checks once W0 lands.
- **Commit per green slice; never push without authorization. Dedicated branch/worktree**
  (`wasm-target`).

## W0 — hold the ground (portability as a CI invariant)

The clean check is an accident until CI enforces it.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W0.0 | CI: `cargo check --target wasm32-wasip1` for the core set | — | Add `wasm32-wasip1` to the toolchain step in `.github/workflows/ci.yml`; check the pipeline crates (`noeta-vm`, `noeta-stdlib`, `noeta-compiler`, `noeta-db`, `noeta-bundle`, `noeta-eval` + transitive). Fast (few seconds warm); keeps a stray `std::thread`/tokio/getrandom from silently landing in core. `unknown-unknown` joins in W2.0 once its getrandom wiring exists. |

## W1 — WASI runner (the edge foundation)

A `wasm32-wasip1` module that runs a `.noeb` bundle under wasmtime. This is the L1/L2 analog:
no codegen, the VM interprets embedded bytecode. wasip1 first (mature tooling, `std` largely
works); the wasip2/`wasi:http` component is W4, where the edge story completes.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W1.0 | `WasiHost` | — | New small crate (e.g. `noeta-wasi-host`) implementing `Host` over what wasip1 gives via `std`: real fs (`std::fs`), env/args (`std::env`), clock (`std::time`), entropy (WASI random via `getrandom`). **Network = clear runtime error in W1** ("networking requires the wasi:http build, see W4") — wasip1 has no standard sockets. Os::exec likewise errors. Synchronous, no tokio. |
| W1.1 | `noeta-wasm-runner` bin target | W1.0 | A `wasm32-wasip1` binary: reads a `.noeb` (argv path or embedded — see W1.2), decodes via `noeta-bundle`, runs on the VM with `parallel_isolates=false`, `WasiHost` by default / `SandboxHost` behind a flag (for the oracle). Two-file deployment (`runner.wasm` + `app.noeb`) works immediately under `wasmtime --dir`. |
| W1.2 | single-artifact `noeta build --wasm` | W1.1 | One `.wasm` you hand to a platform. Recommended mechanism: post-build **data-segment/custom-section injection** into the prebuilt runner (patch the wasm binary; `wasm-encoder`/`walrus`-class rewrite, or a fixed-size reserved region) — no cargo at user build time, mirroring the L2 "staple onto a copy of the toolchain" trick. Composed-toolchain rebuild (PM Phase 3 machinery) is the fallback if patching gets ugly, and is required anyway for W1.4 ring DCE. |
| W1.3 | wasm differential oracle | W1.1 | Conformance corpus through the runner under wasmtime (sandbox configuration), asserted byte-identical to the native VM; `0 skipped`. Shell out to the `wasmtime` binary from `noeta-conformance` (dev-only dep, same posture as the jit-differential). CI job behind a wasmtime install. |
| W1.4 | size budget + ring DCE | W1.2 | Measure the runner (`opt-level = "z"`, `lto`, no mimalloc — wasm uses dlmalloc). Apply the P-AOT per-ring feature scan (`ExtCall` module set → `--no-default-features --features <rings>`) to the runner build. Optional `wasm-opt` pass if the win justifies the tool dependency. Record numbers in this file. |

**Outcome of W1:** `noeta build --wasm app.noe` → one `.wasm` that runs anywhere wasmtime-class
runtimes exist (CLI, CI sandboxes, compute-only edge). Plus a standing oracle that the wasm build
is semantically identical to native.

## W2 — browser toolchain (the playground)

The entire front-end is pure Rust with no OS deps, so the playground is not a toy transpiler —
it is the *real* compiler and VM, client-side, on `SandboxHost`.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W2.0 | `wasm32-unknown-unknown` build wiring | W0.0 | Kill the getrandom blocker: `getrandom_backend="custom"` stub (bcrypt's RNG path is unreachable — salts are Host-supplied) or `wasm_js`. Add the target to the W0 CI check. |
| W2.1 | `noeta-playground` cdylib | W2.0 | wasm-bindgen exports over the existing pipeline: `check(source) → diagnostics JSON` (spans + codes for editor squiggles), `run(source) → {stdout, exit, diagnostics}` on `SandboxHost`, `fmt(source)` via `noeta-fmt`. Direct pipeline calls first; the salsa `noeta-db` graph joins when incremental editing matters. |
| W2.2 | playground web app on noeta.dev | W2.1 | Editor (CodeMirror; the TextMate/tree-sitter grammar work feeds highlighting), diagnostics pane, examples gallery from `examples/`, share-by-URL. **Run the VM in a Web Worker and terminate on timeout** — the standard runaway-loop guard; no VM fuel counter needed. |
| W2.3 | in-browser language smarts (stretch) | W2.1 | `noeta-ide` compiles too (pure): hover/completion/go-to-def exported alongside, giving the playground LSP-grade behavior with zero server. |

**Outcome of W2:** noeta.dev runs the real toolchain in the visitor's tab: type-check squiggles,
formatted code, deterministic sandbox execution — no backend to operate.

## W3 — browser host (interactive playground programs)

| # | Slice | Depends | Notes |
|---|---|---|---|
| W3.0 | JS-backed `Host` | W2.1 | `fetch` for outbound `Network`, `crypto.getRandomValues` for entropy, `performance.now`/`Date.now` for clock, in-memory (or OPFS) fs. Playground gains real `std.http` client demos. |
| W3.1 | async pump | W3.0 | The cooperative scheduler needs a driver for pending async IO (the `ExternIo` seam is the attachment point): export a `step()`/promise-integration loop so `fetch`-backed operations resolve through the browser event loop without blocking the worker. |

## W4 — `wasi:http` component (the edge payoff)

Edge platforms (wasmtime `serve`, Fastly, Spin) speak **wasip2 components** with
`wasi:http/incoming-handler`. Mapping that onto the existing inbound `Network` capability puts
`http.serve(port, handler)` — unchanged user code — at the edge.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W4.0 | wasip2 target + component build | W1.2 | `wasm32-wasip2` (or wasip1 + adapter), `wit-bindgen`/`cargo-component` wiring for the runner. |
| W4.1 | `wasi:http` ↔ inbound `Network` | W4.0 | Implement the incoming-handler world over the same `Request` extern / `http.response` surface the bundled server uses (HTTP-server arc S0–S6); outbound client over `wasi:http/outgoing-handler`, closing W1.0's Network gap. |
| W4.2 | edge deploy proof + docs | W4.1 | One real handler deployed under `wasmtime serve` + one hosted platform; a `docs/` page: "deploying Noeta to the edge". |

## Out of scope (this arc) — each with its revisit condition

- **Direct wasm codegen (L3 analog).** Cranelift can't emit wasm; this needs a new
  `noeta-ir`/bytecode → wasm emitter (reusing the JIT's `plan.rs` eligibility split) with its own
  oracle. Deferred until W1/W2 produce real perf data — wasm engines JIT the interpreter loop, and
  tier-0 is expected to be adequate for playground and edge handlers. Revisit if edge latency
  numbers say otherwise.
- **Wasm-threads isolates.** `SharedArrayBuffer`/wasm-threads exist but drag COOP/COEP headers and
  atomics-enabled rebuilds; cooperative isolates preserve semantics today. Revisit with multi-core
  edge demand (pairs with the deferred multi-core server work).
- **JIT tier under wasm.** No runtime codegen in wasm modules; permanently tier-0 (the engine's
  JIT does the work). Not a revisit — a structural fact.
- **p2p/local-first in the browser.** p2panda-in-wasm is its own arc (§9.15 territory), not a
  deployment-target concern.
