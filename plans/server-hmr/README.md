# Server & HMR — the live-runtime arc

**Goal.** Complete the "live" story: a persistent runtime that survives code edits (hot module
reload), the reactive transport that carries signal changes to a frontend (WS minimal-diff push /
LiveView), the deferred server tail (graceful drain, `--host`, multi-core), and an embed API so a
host process — a game engine's scripting layer being the canonical second consumer — drives the
same live runtime from its own loop.

**The unifying insight: two consumers, one core.** The web dev server and the game engine want the
*same operation* — "here is a new version of the program; swap it into the running session, keep
the state that should survive" — with different drivers. The dev server drives it from a file
watcher and pushes the result to a browser over WS; the engine drives it from its asset pipeline
and keeps calling `update(dt)` at 60fps across the swap. Build the swap once
(backend-/driver-agnostic, on `VmSession`), then each consumer is a thin driver.

**Co-design constraint (user-pinned 2026-07-11).** The reactivity→frontend concept is part of this
arc, not a separate deferral: signals are what the frontend renders (LiveView), and signals are
what HMR preserves — so an edit flows as *swap bodies, keep signal state, flush, diff-push* and
the browser view survives the edit seamlessly. HMR and LiveView share the WS channel and the
flush hook; designing either alone would bake in the wrong shape.

## What we build on (all shipped, verified 2026-07-11)

- **Live module swap in production** — the debug console's `install_fragment`
  (`crates/noeta-vm/src/lib.rs:6265`): typed-arena `Module` snapshots, **stable-prefix superset
  invariant** (proto/global-slot/shape/name indices are never reused — an index minted under any
  earlier module resolves identically under every later one), old frames keep executing while new
  code lands, dispatch re-reads `self.module` at frame transfer.
- **Late-bound dispatch** — top-level `fn`s resolve through global slots *at call time*
  (`Op::CallGlobal`, `vm/lib.rs:6172`); methods through the by-name `(type, method)` map grown in
  place on swap; field access by name with shape-pointer inline caches that self-heal on a shape
  mismatch. Redefining a function body already propagates to existing call sites in a session.
- **Transactional checking** — session-checker C0–C5, merged, REPL-default: an erroring entry
  restores checker+env and commits nothing (`crates/noeta-check/src/lib.rs:306-328`). This is the
  swap gate: compile/type error → keep the old version running.
- **Reactive core with the flush hook** — the graph is per-run `ExtState`
  (`crates/noeta-stdlib/src/reactive.rs:103-124`); a flush knows exactly which nodes recomputed
  (the hook `plans/deferred.md` reserved for the diff layer). Effect disposal exists and is
  leak-oracle-covered.
- **The serve loop** — `crates/noeta-stdlib/src/serve.rs:123-323`: single worker, cooperatively
  concurrent, accept→spawn-handler→reap, drives sub-tasks via `advance_tasks()` each iteration
  (a natural per-iteration safepoint). The handler closure is captured into a ctx slot **once** at
  `serve.rs:141` — a swap must make the loop re-read it (small, load-bearing fix).
- **A native tier designed for this** — JIT/AOT calls route through mutable entry tables by proto
  index; `crates/noeta-jit/src/lib.rs:1003-1008` explicitly reserves re-pointing a proto's entry
  for hot-reload. Not implemented; `install_fragment` currently asserts the JIT unarmed
  (`vm/lib.rs:6266-6270`).
- **Embedding seams** — `Host` is object-safe (12 capability traits, blanket impl,
  `crates/noeta-native/src/host.rs:442`); a static Rust host registers native types/functions via
  `impl Extension` + `install()`. Gaps: no call-by-name into a live session (results come back as
  display strings via `SessionOutput`), `install()` is a once-only process-global `OnceLock`
  (`registry.rs:679`), no typed value bridge.

## Pinned design decisions

1. **HMR state policy — signals are the preserved unit.**
   - `signal(...)`/`cell` state is **preserved across swaps, keyed by definition site** (the
     site-keyed infrastructure the checker already maintains for codegen is the natural key
     source). A re-run of `let count = signal(0)` at an unchanged site rebinds to the existing
     node instead of minting a fresh one.
   - **Effects registered by the old version are disposed and re-registered** by the new run —
     the reactive core's disposal is exactly this operation; the owner-tree deferral does not
     block the flat case.
   - Plain top-level `let` globals **re-initialize** (they are derivable from code; state that
     must survive belongs in a signal/cell — this makes the language rule teachable: *reactive
     state survives edits, plain state doesn't*).
   - This is observable language behavior and gets documented as such (wiki page section), not
     left as a dev-tool quirk.
2. **Layout changes → restart, not migration.** A swap whose diff touches a type's field
   set/order (a different interned shape) or an enum's variants returns
   `SwapOutcome::NeedsRestart` — the driver falls back to a full restart (dev server: automatic,
   ~8ms + compile thanks to the startup cache; engine: host decides). Shapes are content-interned
   `&'static` and never migrate; positional `Op::ExtractField` indices in match lowering make
   old-layout values flowing into new-layout code unsound. **Instance migration is explicitly out
   of scope for this arc** (deferred with rationale, below). Body-level edits — the overwhelming
   majority of edit-loop iterations — hot-swap.
3. **Embed API is UNSTABLE by decision** (user, 2026-07-11): `noeta-embed` ships as an explicitly
   unstable 0.x surface that adapts to our growing needs — no ABI freeze, no stability doc,
   breaking changes allowed between minor versions until a real engine integration has exercised
   it.
4. **JIT policy on swap: tear down and re-warm.** On a successful swap with the JIT armed, drop
   compiled code + call-site ICs + mirror tables and let tiering re-warm (a one-frame/one-request
   hiccup, always sound). The entry-table re-pointing design (`jit/lib.rs:1003`) is the later
   refinement, not v1. Watch-mode may additionally offer `--no-jit` for the fastest edit loop.
5. **Swap safepoint: between dispatches.** The serve loop checks a reload flag once per loop
   iteration; the embed host swaps between `call()`s. In-flight requests/frames finish on the old
   code — *automatically correct* under the stable-prefix invariant (old frames hold old proto
   indices, which stay valid forever).
6. **One WS channel, two event kinds.** The LiveView diff-push and the HMR client protocol share
   the connection: `patch` (minimal state/DOM diff from a flush) and `swap`/`reload` (HMR
   outcome; `reload` when the fallback restart fired). A dev-mode error event carries diagnostics
   for an overlay.

## Architecture

### The swap core (Phase H)

`VmSession::hot_swap(new_source) -> SwapOutcome` on the live session:

```
parse → check (transactional; errors ⇒ Rejected(diags), session untouched)
      → diff top-level defs vs session registry
      → layout-affecting change? ⇒ NeedsRestart(reason)
      → recompile changed fns/methods as NEW protos (stable-prefix extend, as today)
      → store new closures into the EXISTING global slots / method map entries
      → dispose old-version effects; re-run swapped top-level under site-keyed signal rebinding
      → JIT armed? tear down compiled/ICs, re-warm
      → Swapped { changed: [...], preserved_signals: n }
```

Everything down to "recompile as new protos" is composition of shipped machinery
(`SessionCompiler::extend_checked` + `install_fragment`'s table growth). The genuinely new pieces:
the **def-level differ**, the **site-keyed signal rebinding**, the **effect epoch** (tag reactive
nodes with the module version that created them so disposal is precise), and the **layout-change
detector** (compare interned shapes for same-named types across versions).

### Drivers

- **`noeta serve --watch` / `noeta run --watch`** (Phase W): `notify`-based watcher (real-host
  only, out-of-oracle like `RealExecutor`) → debounce → `hot_swap` → on `Swapped` set the reload
  flag the serve loop polls; the loop re-reads the handler global before each accept dispatch. On
  `Rejected`, keep serving the old version and report diagnostics. On `NeedsRestart`,
  re-exec the program (dev server owns the restart; listener rebind is accepted v1 — fd
  preservation across restart is a polish follow-on).
- **`noeta-embed`** (Phase E): `Session::new(host)`, `load(source)`,
  `call(name, &[Value]) -> Result<Value>`, typed `From/TryFrom` conversions for scalars, strings,
  lists, maps, and `@packed` structs (the raw-buffer `PackedView` ABI is the zero-copy entity-data
  path a game engine actually wants), GC-safe retained handles for values the host keeps across
  frames, `hot_swap(source) -> SwapOutcome`, instance-scoped extension registration (kill the
  `OnceLock`). The engine supplies its own `Host` (already works) and its own swap trigger.

### The transport (Phase L)

Connection hijack on the existing serve loop (the `Response` upgrade variant the http-server arc
deliberately left the contract open to) → WS frames → a **flush subscriber**: after each reactive
flush, the set of recomputed nodes (the hook the core already exposes) serializes to a minimal
diff pushed to subscribed clients. The client runtime is a deliberately tiny JS shim (subscribe,
apply patch, request full state on reconnect). HMR events ride the same channel. Sandbox
determinism: a scripted WS client in the request-script model (frames transcript pinned by the
corpus), exactly how `http.serve` itself was made deterministic.

## Slices

**Phase H — hot-swap core** (the shared piece; everything else consumes it)
- **H0** — def-level differ + body-swap: `hot_swap` for changed `fn`/method bodies only
  (rebind global slots / method entries); `Rejected` on check errors (transactional);
  differential oracle: for programs with no retained state, `hot_swap(v2)` ≡ cold-start of v2,
  pinned over a corpus of before/after pairs.
- **H1** — reactive state preservation: site-keyed signal/cell rebinding, effect epochs
  (dispose old, re-register new), top-level re-run semantics; scripted transcripts pin
  "counter survives a handler edit"; leak oracle across N repeated swaps (arena growth is by
  design — pin that per-swap residency is bounded and document the retention model).
- **H2** — layout-change detection → `NeedsRestart`: shape/variant diff across versions,
  including the transitive case (a changed `@packed` struct embedded in another).
- **H3** — swap with the JIT armed: teardown + re-warm (compiled protos, call-site ICs, mirror
  tables); lift the `install_fragment` unarmed assertion for this path; jit-differential stays
  green across swaps.
- **H4** — `SwapOutcome` surface + docs: the retention model and the signals-survive rule as
  documented language behavior.

**Phase W — the dev loop** (web driver; W0 ships value almost immediately)
- **W0** — `noeta run --watch` / `serve --watch` with **full-restart only** (watcher + debounce +
  re-exec; the startup cache makes this a good dev loop on day one, and it is the permanent
  `NeedsRestart` fallback path).
- **W1** — hot path: `Swapped` swaps between loop iterations; the serve loop re-reads the
  handler global (fix the `serve.rs:141` one-shot capture); in-flight requests finish on old
  code; `Rejected` keeps the old version serving with diagnostics to the terminal.
- **W2** — dev UX: clear swap/restart/reject reporting, `--watch` for `noeta serve` documented;
  browser error overlay arrives with L3 (needs the WS channel).
- **W3** — **impact-filtered tier watch** (user-pinned 2026-07-11: generic machinery, not a
  test-runner feature). An **impact engine** — one library seam, deliberately runner-agnostic:
  input = the H0 differ's changed/added/removed definition names; compute the **reverse
  transitive closure** over the existing call graph (`noeta_ide::callgraph`, the `trace`/
  call-hierarchy extraction) to the set of impacted declarations. Consumers filter that set to
  *their* tier's fns and rerun only those:
  - `noeta test --watch` — impacted `@test` fns via the runner's existing `--name` filter (the
    first consumer and the proof of the seam);
  - `noeta bench --watch` — impacted `@bench` fns (same shape, different tier);
  - **third-party tiers** — the impact query is exposed where extension-contributed commands
    (`ExtCommand`) can reach it, so any custom tier runner a package ships gets change-driven
    reruns for free. The seam's contract is "changed defs → impacted decls"; what a tier does
    with the set is the runner's business.
  Soundness valves are part of the contract, not the consumer's job: a change the differ cannot
  attribute (layout/signature/namespace/top-level, manifest or non-entry files) degrades to
  IMPACT-ALL (full rerun), and the engine reports *why*; a static call graph cannot see
  reflection/`invoke`/method-handle dispatch, so consumers surface an occasional/on-demand full
  pass (the engine flags declarations reached only dynamically as best-effort). Oracle: impact
  results pinned against hand-computed closures on a fixture graph; end-to-end — edit a leaf fn,
  exactly its caller-tests rerun; edit a layout, everything reruns.

**Phase L — reactive transport + HMR client** (the LiveView half)
- **L0** — WS upgrade + frame codec on the serve loop (hijack path), sandbox scripted-client
  determinism, echo conformance.
- **L1** — flush subscriber → minimal-diff protocol: recomputed-node set → diff frames;
  deterministic transcript in-oracle; fan-out bench (reuse the reactivity flush bench shape).
- **L2** — the tiny client runtime + a LiveView example app (server-rendered state, signal-driven
  patches); doc-samples gated.
- **L3** — HMR over the channel: `swap`/`reload`/`error` events; the showcase demo — edit a
  handler/computed, browser view updates in place with signal state intact.
- **L4** — reactive-flush telemetry tie-in (the OTEL arc's deferred T5e hooks in naturally here;
  keep lean).

**Phase E — embed** (engine driver; independent of W/L after H, can run as a parallel session)
- **E0** — `noeta-embed` crate: session lifecycle, `call(name, args)` (expose the `vm.call_value`
  seam the serve loop already uses), typed value bridge + retained handles; Rust integration
  tests, round-trip property tests.
- **E1** — instance-scoped extensions (replace the `OnceLock` global with per-session registry;
  `install()` stays as a compat shim over a default instance).
- **E2** — `hot_swap` exposed + the 60fps proof: a host loop calling `update(dt)` across a swap
  without a dropped frame budget (bench-pinned), `@packed`/`PackedView` zero-copy entity buffer
  example.
- **E3** — the engine-shaped demo: custom `Host` + custom `Extension` (engine API surfaced to
  scripts) + hot-swap on file change; doubles as the embed-API shakedown that decision 3 wants
  before any stability talk.

**Phase S — server tail** (completes the http-server deferrals)
- **S0** — graceful drain on SIGINT (shutdown flag in the loop, in-flight reaped, listener
  closed) + `--host` (thread the bind address through).
- **S1** — multi-core `noeta serve --parallel N`: acceptor isolate + fd-over-`Channel<int>` to
  worker isolates (the design pinned in `plans/deferred.md` — no `SO_REUSEPORT`, no new dep);
  **swap broadcast** (a swap must reach every worker isolate — H's swap core takes a "notify
  isolate" seam now so single-worker doesn't bake in a one-VM assumption).

Suggested order: H0→H1→W0 (early value)→W1→H2→H3→L0→L1→L2→L3→H4/W2→W3, with E running in
parallel after H1 and S last (S1 interacts with everything and wants the swap core settled).
W3 sits after the L phase by default (the transport unblocks two consumers; W3 improves one
loop that already works correctly-if-wastefully) but depends only on H0 + the existing call
graph — it can be pulled earlier or run as a parallel session's slice if test-feedback latency
starts to matter more.

## Oracle posture

- **Swap differential (the spine):** `hot_swap(v2)` ≡ cold-start(v2) for stateless programs,
  byte-identical `RunResult`, over a before/after corpus — this makes the differ and rebinder
  refactorable, not risky.
- **State-preservation transcripts:** scripted sessions (run, mutate signals, swap, observe) pin
  the retention semantics exactly; these are the language-behavior tests, not just tool tests.
- **Serve + swap in-oracle:** extend the sandbox request-script model with a mid-script swap
  event, so "request → edit → request" is deterministic and differential-covered.
- **WS determinism:** scripted client, pinned frame transcripts (the request-script trick again).
- **Leak oracle across swap churn** (old protos/shapes are retained by design; the assertion is
  *bounded per-swap growth*, documented) + miri over the new graph-epoch code.
- **jit-differential green across swaps** (H3); embed round-trip property tests (E0);
  fan-out diff-push bench (L1); 60fps swap-hiccup bench (E2).
- Standard gates every slice: conformance + differential 0-skipped, `cargo test --workspace`,
  clippy, fmt, doc-samples where docs change.

## Deliberate non-goals (recorded, not silently cut — flagged for explicit sign-off)

- **Instance migration on layout change** — restart is the fallback (decision 2). Migration
  (shape-to-shape value mapping, user migration hooks) is a research-grade follow-on; the
  detector (H2) is where it would later attach.
- **C ABI / dynamic loading for non-Rust hosts** — embed is Rust-static (`impl Extension` +
  linking), consistent with the package-manager Phase-3 confirmed model. A C shim is a future
  arc if a non-Rust engine materializes.
- **JIT entry-table re-pointing** — teardown/re-warm first (decision 4); the design note stands.
- **Browser DOM framework** — L2's client is a minimal patch-applier, not a component framework;
  LiveView composes from in-language code the way routing/middleware did (the S5 composability
  proof pattern).
- **Multi-core × LiveView session stickiness** — with `--parallel N`, WS subscribers and signal
  state are per-worker-isolate; v1 documents "LiveView apps run single-worker" and S1 records
  the sticky-routing/shared-state question as its own follow-on rather than solving it here.
- **Listener-fd preservation across `NeedsRestart`** — v1 rebinding drops the port for ~10ms;
  fd handoff is polish.

## Open questions (to resolve in-slice, flagged early)

- **Site identity for signal keying** across edits: a pure (module, AST-path) key shifts when
  code above it moves; line-insensitive structural keys (def-path + occurrence index within the
  def) are likely right — H1's first decision, informed by how the checker's `Sites` already
  keys things.
- **`use`-graph granularity:** v1 swaps at whole-program granularity (recompile the file +
  its project modules through the normal loader); per-module blast-radius via the salsa
  boundary (the `plans/m1/slice-09` note) is the optimization, not the semantics.
- **What "top-level re-run" means for imports with side effects** (e.g. a top-level
  `http.serve` call itself): the serve entry is driver-owned in watch mode, so the synthesized
  entry call is excluded from re-run; any other blocking top-level call in a swapped module is
  reported, not re-run (needs a rule by H1).
