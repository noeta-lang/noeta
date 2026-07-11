# WASM target arc (P-WASM) — playground + edge

**Status: W0 + W1 COMPLETE (branch `wasm-target`) — the edge foundation ships; W2 (playground) next.** Goal: make wasm a first-class deployment target driven by two
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
| W0.0 | CI: `cargo check --target wasm32-wasip1` for the core set | — | ✅ **DONE** (`100f4ffd`). New `wasm` CI job checks the nine pipeline crates on wasip1, `--locked`. `unknown-unknown` joins in W2.0 once its getrandom wiring exists. |

## W1 — WASI runner (the edge foundation)

A `wasm32-wasip1` module that runs a `.noeb` bundle under wasmtime. This is the L1/L2 analog:
no codegen, the VM interprets embedded bytecode. wasip1 first (mature tooling, `std` largely
works); the wasip2/`wasi:http` component is W4, where the edge story completes.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W1.0 | `WasiHost` | — | ✅ **DONE** (`43d3e547`). `noeta-wasi-host`: the third `Host`, real-but-synchronous — std fs (lazy P-LAZY readers), env overlay, wall clock/entropy, seeded PRNG + logical monotonic (the RealHost rules), loopback `P2pBroker`, null-sink telemetry that still tracks live-span contexts. Network/`os.exec` = honest errors. Note: Rust leaves `env::consts::OS` **empty** on wasm targets — `os_platform` names `"wasi"` itself. |
| W1.1 | `noeta-wasm-runner` bin target | W1.0 | ✅ **DONE** (`ece433f3`). Target-agnostic bin: preopened `.noeb` → `noeta-bundle` decode → `run_module_debug(…, None)` (the documented plain-run-plus-traceback entry; cooperative, tier-0). `--sandbox` pins the oracle configuration. Diagnostics/trace render against the synthetic empty source (`noeta run app.noeb` convention). Verified under wasmtime 46: real fs writes, uuids, panic tracebacks, exit codes. 2.4 MB release module. |
| W1.2 | single-artifact `noeta build --wasm` | W1.1 | ✅ **DONE**. `noeta_bundle::staple_wasm` — a **dependency-free section-level rewrite** of the raw binary (~1 ms; the first cut used walrus, whose full IR round-trip cost ~1.2 s/staple — 5× the whole oracle — so walrus was demoted to a dev-dependency that builds/re-validates test modules): the bundle rides a **new active data segment at the old memory end** — provably disjoint from data/shadow-stack/heap, since Rust's wasm allocator acquires pages via `memory.grow`, whose first call returns the *bumped* minimum — and a magic-tagged slot static in the runner (`embedded.rs`) is patched with `(ptr, len)`; every other section copies byte-for-byte. The runner reads the slot with `read_volatile` (a plain read of the immutable static would constant-fold to the unpatched zeros) — one quarantined `unsafe`, oracle-gated like the JIT's native code. CLI: `noeta build --wasm` → one `.wasm`, `wasmtime run app.wasm [args…]`, argv owned by the program (`--exe` convention); sandbox configuration via `NOETA_WASM_SANDBOX=1` (a stapled artifact's argv cannot carry a flag). Runner discovery mirrors `resolve_aot_runtime`'s ladder (env → exe-adjacent → interim workspace cargo build). The W1.3 oracle runs every corpus case in **both shapes**: 576 matched / 0 skipped / 0 divergences, ~2.5 min. |
| W1.3 | wasm differential oracle | W1.1 | ✅ **DONE**. `noeta-conformance --wasm-differential`: corpus → bundle → runner under wasmtime (`--sandbox`) vs native `run_module_traced`; compares stdout, exit byte, and stderr composed through the *same* rendering calls (`render_mapped` + `render_trace`). First full run: **576 matched, 0 skipped, 0 divergences** (~15 s with `-C cache=y`). Tool discovery via `NOETA_WASMTIME`/`NOETA_WASM_RUNNER`; missing tooling = loud exit 2, never a silent pass. Wired into the CI `wasm` job. |
| W1.4 | size budget | W1.2 | ✅ **DONE** (measured on wasmtime 46, warm `fib(31)`): plain release **2.42 MiB**; **`wasm-release` profile (release + `strip`) = 2.15 MiB, identical speed** — the shipped default (the stripped 220 KiB `name` section only fed wasmtime's trap backtraces; Noeta tracebacks come from the bundle's line table). Full size build (`opt-level=z` + fat LTO + `panic=abort`) = **1.37 MiB but ~60% slower** (0.51 s → 0.82 s) — REJECTED as default; size-critical deploys can build that shape and point `NOETA_WASM_RUNNER` at it. **Ring DCE is N/A for wasm**: the heavy rings (reqwest/p2panda/Cranelift) live in `noeta-runtime`/`noeta-jit`, which the runner never links — the P-AOT footprint problem does not exist here. `wasm-opt` not adopted (no tool dependency for single-digit % on an already-small artifact). |

**Outcome of W1:** `noeta build --wasm app.noe` → one `.wasm` that runs anywhere wasmtime-class
runtimes exist (CLI, CI sandboxes, compute-only edge). Plus a standing oracle that the wasm build
is semantically identical to native.

## W2 — browser toolchain (the playground)

The entire front-end is pure Rust with no OS deps, so the playground is not a toy transpiler —
it is the *real* compiler and VM, client-side, on `SandboxHost`.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W2.0 | `wasm32-unknown-unknown` build wiring | W0.0 | ✅ **DONE**. `.cargo/config.toml`: `--cfg getrandom_backend="custom"` for the target; the playground crate defines the hook as honestly-unsupported (bcrypt's salt path is unreachable — salts are Host-supplied). No `wasm_js`/wasm-bindgen creep. The full engine — salsa graph, VM, formatter — checks clean on the target with just the cfg. |
| W2.1 | `noeta-playground` cdylib | W2.0 | ✅ **DONE**. Salsa-backed (the IDE-engine path, so W2.3 smarts are additive) `check`/`run`/`fmt` → JSON: `check` emits the stable `JsonDiagnostic` shape (`noeta check --format json`), `run` executes on the deterministic `SandboxHost` via `run_module_traced` (stdout/exit/diagnostics + rendered traceback). Exports are a **hand-rolled `(ptr,len)` C ABI** (length-prefixed results) — deliberately no wasm-bindgen; three string→string exports don't justify a version-locked codegen tool. 5 native unit tests + a **node smoke test over the raw ABI** (salsa runs fine in a plain JS embedding — no imports, no clock panic), both in the CI wasm job. Artifact: 3.9 MiB `wasm-release`. Runaway guard = embedder's Web-Worker-terminate (by design, no VM fuel). |
| W2.2 | playground web app | W2.1 | ✅ **DONE (first cut)**: `web/playground/` — three dependency-free static files (no bundler, no CDN, works offline) + the engine artifact. Worker owns the wasm (streaming instantiation with buffered fallback); main thread terminates it after 5 s and respawns (the runaway guard). Editor (textarea) + output pane + diagnostics pane with jump-to-span links (byte offsets from `JsonDiagnostic`), examples picker, Ctrl+Enter, dark/light. Serve: `python -m http.server -d web/playground`. **Deferred to the noeta.dev pass** (not silently — needs a frontend session): CodeMirror + tree-sitter highlighting, share-by-URL, examples from `examples/`. |
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
