# WASM target arc (P-WASM) — playground + edge

**Status: W0–W4 COMPLETE (branch `wasm-target`) — both use cases delivered: single-artifact edge binaries + `wasi:http` serve components, and the in-browser playground with IDE-grade smarts. Remaining: recorded follow-ups (W3.1 pump, component staple, outgoing-handler, docs/hosted-platform pass, CodeMirror UI).** Goal: make wasm a first-class deployment target driven by two
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
| W2.2 | playground web app | W2.1 | ✅ **DONE (first cut)**: `web/playground/` — three dependency-free static files (no bundler, no CDN, works offline) + the engine artifact. Worker owns the wasm (streaming instantiation with buffered fallback); main thread terminates it after 5 s and respawns (the runaway guard). Editor (textarea) + output pane + diagnostics pane with jump-to-span links (byte offsets from `JsonDiagnostic`), examples picker, Ctrl+Enter, dark/light. Serve: `python -m http.server -d web/playground`. **Hosting prep landed ahead of the noeta.dev pass**: `web/playground/build.sh` → `dist/` (engine + statics + brotli side-cars, 5.0 MiB → 960 KiB), **share-by-URL** (`share.js`, versioned base64url fragment, pure + node-tested), and CI uploads `playground-dist` + the generic wasm engines as artifacts every run — deploying is "download + upload" the day hosting exists. **Still deferred to the frontend session**: CodeMirror + tree-sitter highlighting, the IDE-smarts UI, examples from `examples/`. |
| W2.3 | in-browser language smarts | W2.1 | ✅ **ENGINE HALF DONE**: `noeta-ide` compiles clean for the browser target (it is wire-protocol-free by design — `noeta lsp` is just its tower-lsp adapter), and the playground now exports `noeta_hover`/`noeta_definition`/`noeta_complete`/`noeta_signature` over the same ABI — the LSP's answers with zero server. One persistent `DocumentStore` per instance, fed through `change()` (the keystroke path), so the browser gets salsa incrementality for free; sibling discovery degrades gracefully to single-file (`std::fs` errors on the target). Positions are zero-based UTF-16 `(line, character)` — the LSP convention, native to JS editors. Native unit tests + node smoke over the raw ABI. **UI half (hover popups / completion menus) needs a real editor component — lands with the CodeMirror/noeta.dev frontend pass** (a `<textarea>` cannot render them). Artifact 4.3 MiB. |

**Outcome of W2:** noeta.dev runs the real toolchain in the visitor's tab: type-check squiggles,
formatted code, deterministic sandbox execution — no backend to operate.

## W3 — browser host (interactive playground programs)

| # | Slice | Depends | Notes |
|---|---|---|---|
| W3.0 | JS-backed `Host` | W2.1 | ✅ **DONE**. `BrowserHost` — the fourth `Host`, in `noeta-playground` — backs the real-world leaves with **wasm imports the worker supplies** (`noeta_host` module): entropy ← `crypto.getRandomValues`, wall clock ← `Date.now`, and outbound HTTP ← **synchronous XMLHttpRequest** — legal precisely because the engine only ever runs in a Web Worker, which lets the synchronous `net_fetch` leaf reach the real network with *zero* VM seam changes. Requests/replies cross the import as JSON in the export surface's own length-prefixed packing. Fs stays in-memory (`Vfs`), PRNG seeded / monotonic logical (the every-host rules), inbound serving + `os.exec` honest errors. New `noeta_run_browser` export; page gains a "real host" toggle + an `http fetch` example. The shared null-sink [`SpanTracker`] was extracted into `noeta-native` (rule of three: WasiHost, BrowserHost, and any future no-exporter host) and `noeta-wasi-host` refactored onto it. Node smoke drives a full `std.http` round trip through the import contract + real-vs-deterministic uuid assertions. |
| W3.1 | async pump (JSPI) | W3.0 | ✅ **DONE — zero ABI changes, on the exact seam `RealExecutor` uses.** `BrowserExecutor`: `spawn_ext` takes the descriptor's `run_real` body — `BrowserHost::net_spawn` hands out a `BrowserFetchIo` whose async body is a plain Rust future over a **JS fetch ticket** (`js_fetch_start` begins `fetch()` without suspending; `js_fetch_take` polls) — and polls it once, so N spawns put N requests in flight; `advance` parks the whole wasm stack on the ONE suspending import (`js_wait` = `Promise.race([any ticket settles, earliest timer])`) while the event loop runs. `now()` is elapsed real ms, so async `sleep` deadlines are real time. New `noeta_run_browser_async` entry, wrapped with `WebAssembly.promising`; the worker feature-detects JSPI and falls back to the serial entry (the same descriptor's `run_sync` body — one code path, two degradations). **Overlap proven headlessly**: node 26 ships JSPI unflagged, and the smoke asserts both fetches started before either settled (controlled resolution). CI pins node 26. War story recorded in the future's doc comment: the first draft re-fired the fetch every poll (the request was put back for its URL) — caught because the debug counters showed 5,733 starts for one await, and cross-checked against the real host (exactly 1 request) before touching the VM. |

## W4 — `wasi:http` component (the edge payoff)

Edge platforms (wasmtime `serve`, Fastly, Spin) speak **wasip2 components** with
`wasi:http/incoming-handler`. Mapping that onto the existing inbound `Network` capability puts
`http.serve(port, handler)` — unchanged user code — at the edge.

| # | Slice | Depends | Notes |
|---|---|---|---|
| W4.0 | wasip2 target + component build | W1.2 | ✅ **DONE**. No `cargo-component`/`wit-bindgen` needed: Rust's native `wasm32-wasip2` target emits a component directly, and the `wasi` crate (0.14) provides the proxy-world bindings + `export!` macro. `noeta-wasm-serve` (cdylib+rlib): the target-agnostic core (request → per-request VM run → response over neutral `NetRequest`/`NetResponse`) is natively unit-tested; the `wasi:http` type glue is `cfg`'d to the wasi target. The program originally baked in at build time via `include_bytes!`; **superseded by staple-into-component** (see the follow-ups row below) — the serve crate now carries the same patchable slot as the runner, and `noeta build --serve` staples in ~1 ms. |
| W4.1 | `wasi:http` ↔ inbound `Network` | W4.0 | ✅ **DONE — zero VM changes.** The inversion: a wasi:http component is invoked *per request*, and the sandbox already models inbound serving as a **finite request script that ends the serve loop**. So `WasiHost` gained `with_inbound(request) → (host, ReplySlot)`: `net_listen` arms, the first `net_accept_next` yields the one request, the next yields `None` (the serve loop returns), `net_reply_now` lands in the shared slot (the sandbox's sink pattern — the host is consumed by the VM). Unchanged `server.serve(port, handler)` programs work verbatim; a program that never replies answers a diagnostic **500** carrying its stdout/diagnostics. Per-request VM instantiation matches the platform's own per-request component model. Outbound client over `wasi:http/outgoing-handler` (closing W1.0's gap) = follow-up. |
| W4.2 | `noeta build --serve` + e2e | W4.1 | ✅ **DONE**. CLI verb: compile → bundle → cargo-bake the component (`wasm32-wasip2` + `wasm-release`) → `<app>.serve.wasm`; deploy with `wasmtime serve -S cli=y app.serve.wasm` (`-S cli=y` because Rust std imports the cli world beyond the proxy world). E2E proven live: `curl /ping` → `200 "edge says hi: /ping"`; scripted as `crates/noeta-wasm-serve/tests/e2e.sh` in the CI `wasm` job. ✅ Docs + second-engine proof DONE: `docs/WebAssembly-and-the-Edge.md` (indexed in Home/Sidebar/The-CLI; serve samples `ignore`-tagged — the doc gate runs on the real host and they bind real sockets; the e2e script is their stronger gate). **The same artifacts serve and proxy under Spin 4 unmodified** (spin.toml + `allowed_outbound_hosts`), added as an optional e2e leg (runs when `spin` is on PATH; wasmtime legs stay the required gate — CI stays spin-free). A true cloud deploy (Fermyon/Fastly) needs an account and stays a user action; the docs give the commands. |
| W4-F1 | `wasi:http/outgoing-handler` | W4.1 | ✅ **DONE**. `WasiHost::with_outbound(hook)` — the platform's HTTP client as an injected closure (the host crate stays wasi-crate-free); the serve component passes the outgoing-handler dance: URL → scheme/authority/path-with-query, body streamed, then the wasip2-native wait — **block on the response future's pollable** (the mirror of the JSPI pump's `js_wait`, one level down). Edge handlers compose upstream services: proven live (`edge proxied: 42 from upstream`) + e2e'd + native canned-hook test. Closes the Network capability's last honest-error on the serve path. |
| W4-F2 | staple-into-component | W4-F1 | ✅ **DONE — `noeta build --serve` went from ~15 s + cargo + source tree to ~0.2 s of binary surgery.** The component binary format shares the core format's top-level `(id, size, contents)*` shape (verified against the real artifact: three embedded core modules, each with its own full header); `staple_wasm` now detects the version/layer bytes and, for a component, patches the ONE embedded module carrying the slot (ambiguity across modules refused) and re-emits around it. The serve crate switched from `include_bytes!` to the runner's slot mechanism (`read_volatile`, second quarantined-unsafe twin); `--serve` gained `resolve_wasm_runner`'s ladder (`NOETA_WASM_SERVE` → exe-adjacent → interim workspace build of the *generic* component, built once and cached). Unit-tested on synthetic components + the real artifact; both e2e cases run through stapling. **War story: the first CLI wiring silently no-op'd (a whole-function text replace missed after a fmt reflow) and shipped the old copy-not-staple path — caught because the e2e asserts *runtime* behavior, and replaced with a line-anchored edit.** |

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
