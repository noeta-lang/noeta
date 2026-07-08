# Native OTEL — plan

**Branch:** `native-otel` (off `main` @ `9bd38e1`, which includes the higher-order-abi merge H0–H7).
**Worktree:** `.claude/worktrees/native-otel`. **Roadmap line:** `plans/roadmap.md:20` — the
production-observability half of the split "observability" item (the dev-profiler half shipped as
`noeta profile`, [`plans/profiler/README.md`](../profiler/README.md)).

## What it is

Production observability: distributed **tracing** for Noeta programs, emitted as OpenTelemetry.
Spans wrap units of work (a request, a task, an effect flush); each carries a name, timing,
attributes, events, status, and a parent link; trace context propagates across isolates and the
wire (W3C `traceparent`). Export is real-host-only (like Network) — **never differential-tested**,
because a span is a *write-only side effect* invisible to program output.

**Scope of this arc: tracing only.** Metrics (counters / histograms / gauges) and logs are a
natural OTel sibling but a materially larger surface — deferred to a follow-on arc (see *Deferred*).

## What higher-order-abi unblocked (why now, one branch)

The roadmap deferred both observability halves because they "touch crates under active refactoring
by a parallel effort" (`roadmap.md:22`) — that effort was higher-order-abi, now merged. Everything
this arc leans on is in `main`:

- **`ExtState` + retained arena** (`noeta-native/src/ctx.rs:46,220-237`) — a stateful extension
  holding language values across calls, mirrored on `noeta-stdlib/src/reactive.rs` (the proving
  Class-3 client: `state_of` `:112`, `retain`/`retained_get`/`retained_set` `:178,255,276`).
- **`ctx.call`** (`ctx.rs:123`) — re-enter the interpreter to run a closure, which makes the
  scoped `with_span(name, body)` form possible (the answer to the no-RAII constraint below).
- **ctx orchestration** (`ctx.rs`: `advance_tasks:174`, `poll:157`, `spawn_io:146`) — the seam the
  `serve` accept loop already rides (`noeta-stdlib/src/serve.rs:92`), where request auto-instrumentation
  injects.
- **Generic/ctx-form extern-type method dispatch** — the `Span` extern type with effectful methods.
- **Extension commands** (`noeta-stdlib/src/registry.rs:35`) — `noeta serve` is now an extension
  command (`serve.rs:45` `SERVE_COMMAND`); the tracing wrapper lands there, not in core.

The bundled HTTP server is also in `main` now (the roadmap's "soft-blocked on the server" caveat is
lifted — the headline use case, tracing inbound requests, is buildable).

## Architecture — SDK-as-extension + capability-as-sink (the HTTP parallel)

Telemetry mirrors HTTP exactly: an in-language SDK extension over a real-IO Host capability.

| | The SDK (in-language ergonomics) | The sink (real IO) |
|---|---|---|
| **HTTP** | `std.http` extension | `Network` Host capability |
| **Telemetry** | `std.telemetry` extension | `Telemetry` Host capability (**8th**) |

### `Telemetry` Host capability

An 8th capability sub-trait added to the `Host` union (`noeta-native/src/host.rs:190-191`), so the
compiler forces both backends to implement it. Sketch (names provisional; finalized in T0):

```rust
// noeta-native/src/host.rs
pub trait Telemetry {
    /// Start a span; parent = the capability's current active span unless `remote` overrides it.
    fn tel_span_start(&mut self, name: &str, kind: SpanKind,
                      remote: Option<TraceContext>) -> SpanId;
    fn tel_span_set_attr(&mut self, span: SpanId, key: &str, value: AttrValue);
    fn tel_span_add_event(&mut self, span: SpanId, name: &str, attrs: &[(String, AttrValue)]);
    fn tel_span_set_status(&mut self, span: SpanId, status: SpanStatus);
    fn tel_span_end(&mut self, span: SpanId);
    /// W3C traceparent of the current active span — the propagation read.
    fn tel_current_context(&mut self) -> Option<TraceContext>;
    /// Push/pop the active span (what `with_span` brackets, so `span()` children nest correctly).
    fn tel_activate(&mut self, span: SpanId);
    fn tel_deactivate(&mut self, span: SpanId);
}
```

`SpanId`/`TraceContext`/`AttrValue`/`SpanKind`/`SpanStatus` are plain `Send` data in `noeta-native`
(no backend or OTel-crate types leak into the ABI — `TraceContext` is trace-id + span-id + flags,
serializable to/from a `traceparent` string).

- **Sandbox impl** (`noeta-stdlib/src/host.rs`) — a **deterministic in-memory recorder**: pushes
  `SpanData` into a `Vec`, timestamps from the logical clock (`clock_unix_ms`, `host.rs:88`), IDs
  derived from the seeded stream (`entropy_u64`, `host.rs:98`). Enables conformance assertions on
  emitted spans without a live collector; **in-oracle by construction** (see *Differential* below).
- **Real impl** (`noeta-runtime/src/lib.rs`, behind the `telemetry` feature) — wraps the
  `opentelemetry` + `opentelemetry_sdk` span model + batch span processor + a **custom OTLP/HTTP-JSON
  `SpanExporter`** over the existing `reqwest`+`rustls` stack (see *Bundle* — no `opentelemetry-otlp`
  / `tonic` / `prost`). Active-span context is per-isolate host state (each isolate already gets its
  own `RealHost` via `HostFactory`, `noeta-vm/src/session.rs:34`). **Flush on `Drop`/`shutdown()`** —
  the exporter drains batched spans when `RealHost` drops at run teardown; **no new VM hook needed**.

### `std.telemetry` extension (thin facade, `noeta-stdlib`)

Mirrors `reactive.rs`'s Class-3 shape; marshals to the Host capability, adds the ergonomic surface:

- **Module functions**
  - `span(name: str) -> Span` — start a span parented on the current active.
  - `with_span(name: str, body: Fn() -> T) -> T` — **scoped span via `ctx.call`** (`ctx.rs:123`):
    start → activate → run `body` → deactivate → **end, even on abort** (the dispatch turns
    `CtxError::Abort` into end-with-error-status then re-propagates, as `serve` turns a handler abort
    into a 500, `serve.rs:57`). This is the answer to the no-RAII constraint.
  - `current_context() -> str` — the current `traceparent` (plain string → **Wire-safe** for
    cross-isolate/HTTP propagation).
  - `span_from(name: str, traceparent: str) -> Span` — start a span with a *remote* parent (the
    extract side of propagation).
- **`Span` extern type** — mutable/effectful, non-key-capable, like `FileHandle`
  (`noeta-stdlib/src/handle.rs`). Payload = a plain `SpanId`. Methods marshal to `host.tel_*`:
  `set_attribute(k, v)`, `add_event(name)`, `record_error(e)`, `set_status(ok|error, msg)`, `end()`.

Registered like `http`: an `ExtModule` in `STD_MODULES` and the `Span` `ExtType` in `STD_TYPES`
(`registry.rs:23-69`, `2047-2054`); the ctx-form dispatch (`ctx_dispatch`/`ctx_functions`) since
`with_span`/`Span` methods re-enter the interpreter and touch the Host.

### Crate homes (confirmed against the merged tree)

| Crate | Adds |
|---|---|
| `noeta-native` | `Telemetry` trait + into the `Host` union; `SpanId`/`TraceContext`/`AttrValue`/`SpanKind`/`SpanStatus`/`SpanData` ABI data types. Zero new deps. |
| `noeta-stdlib` | sandbox recorder (`host.rs`); `std.telemetry` module + `Span` `ExtType` (new `telemetry.rs`); registry wiring. Zero heavy deps. |
| `noeta-runtime` | real OTLP/JSON exporter (`telemetry` feature) — the only crate with new deps (`opentelemetry`, `opentelemetry_sdk`). |

## Key decisions

**No-RAII → scoped `with_span`.** Freeing an extern value is a plain Rust drop and the GC cannot
reach the Host at free time (`extern_value.rs:15-18`), so a `Span` cannot auto-flush on drop — it
needs an explicit end, exactly like `FileHandle.close()`. The primary API is therefore the scoped
`with_span(name) { … }` (ends on scope exit, abort-safe); the bare `span()` + `.end()` is the
lower-level escape hatch for spans that outlive a lexical scope.

**Differential-clean by construction.** Telemetry is write-only — spans never re-enter program
output — so the two backends produce identical `RunResult`s regardless of what they emit. The
capability flows through `Box<dyn Host>` (sandbox injected in the oracle, real host CLI-only), and
the sandbox recorder is deterministic, so telemetry is in-oracle the way Network is, without the
oracle ever comparing span contents. Conformance asserts on the sandbox recorder through a
test-only introspection path (as the http sandbox responder is tested).

**Propagation is explicit serialization** (the OTel model, and it fits the isolate boundary): the
active context lives in per-isolate Host state and is **not** `Wire`-able as a live object (like
signals). Crossing an isolate or the wire = `current_context()` → a `traceparent` string (plain,
Wire-safe) → `span_from(name, traceparent)` on the far side. Auto-instrumentation does this
injection/extraction for the user at the server, task, and isolate-message boundaries.

**Bundle: lean exporter + feature gate + footprint ring.** A non-telemetry user must not pay for
the OTel tree. Three mitigations:
- The trait + sandbox recorder carry **no heavy deps** — free for everyone.
- The real exporter uses **`opentelemetry` + `opentelemetry_sdk` (no protobuf) + a custom OTLP/HTTP-JSON
  `SpanExporter` over the already-compiled `reqwest`+`serde_json`** — deliberately **not**
  `opentelemetry-otlp` (which drags `tonic`/`prost`). Net new tree ≈ the two lean API crates.
- It sits behind a **`telemetry` cargo feature** (on in the default dev build; a minimal CLI can
  drop it) and is a **footprint-selected stdlib ring** — linked into a `noeta build --native` binary
  only when the program imports `std.telemetry`. That selection is the parked AOT-DCE mechanism
  ([`plans/aot/dce.md`](../aot/dce.md), branch `aot-dce`, "feature-gate + footprint-selected
  archive"). Until it lands, telemetry rides the *same* "unused Ring-2 module still linked"
  limitation as `json`/`crypto` — not a new category of cost, just the heaviest single instance,
  which strengthens the case for finishing that DCE work. Trade-off accepted: we hand-write the
  OTLP/JSON exporter (span→JSON mapping + batched POST) rather than take it off-the-shelf; the SDK
  still supplies the batch processor and span model, so the hard parts aren't reinvented.

**Configuration.** Endpoint/headers/service-name from the standard env vars
(`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, `OTEL_EXPORTER_OTLP_HEADERS`) read through the
`Env` capability; a `telemetry.configure(...)`-style in-language override is a possible T2 addition.
Unconfigured (no endpoint) → the exporter is a **null sink**: `tel_*` short-circuit before any work,
so tracing calls in un-configured programs are ~free (the perf gate below).

## Slices (commit per green slice; full gate = workspace suites + differential + leak + conformance + fmt + clippy)

- **T0 ✅ DONE (`7343956`).** `Telemetry` trait + neutral ABI data types
  (`SpanId`/`SpanKind`/`SpanStatus`/`AttrValue`/`TraceContext`/`SpanData`/`SpanEvent`, W3C
  `traceparent` round-trip) in `noeta-native`, added to the `Host` union so both backends are
  compiler-forced to implement it. The capability is a pure span factory/sink (start/set-attr/
  add-event/set-status/end + a live span's `TraceContext`); the active-span stack is deferred to the
  SDK extension (T1+), keeping both host impls simple. `SandboxHost` = deterministic in-memory
  recorder (counter-derived ids, logical-clock timestamps; test introspection via `recorded_spans`).
  `RealHost` (behind default-on `telemetry`) = real-entropy ids + a hand-rolled **OTLP/HTTP-JSON**
  exporter over the reqwest+serde_json already compiled (no `opentelemetry-otlp`/tonic/prost);
  env-configured, null sink when unset, buffer+flush at a threshold / on teardown. Gate: runtime+
  stdlib unit tests (OTLP/JSON shape, deterministic recorder, traceparent), conformance
  differential/leak/trace-parity, clippy, and both feature-on/`--no-default-features` builds — all
  green. No language surface yet (T1).
- **T1 ✅ DONE (`89a037e`).** `std.telemetry` module: `span(name) -> Span` + `Span` extern type
  (mutable/effectful, non-key, like `FileHandle`) with `set_attribute` (chaining) + `end`, marshalling
  to `host.tel_span_*`. Plain dispatch (no closures/state), registered in `STD_MODULES`/`STD_TYPES`;
  the checker **auto-derives every signature from the registry** and auto-reserves `Span` — **zero
  `noeta-check` edits**. `set_attribute`'s value is a scalar union (`string|int|float|bool`), so a
  non-scalar is a compile-time **E0007** and a user `struct Span` is **E0049** (both pinned as
  diagnostics corpus entries). *Gate:* conformance differential (`basic_span.noe` runs identically on
  both backends; leak-0 — the `Span` extern releases at scope end) + the two diagnostics entries; a
  noeta-stdlib unit test drives dispatch and asserts on the sandbox recorder; 84 stdlib tests + clippy
  green. Implicit parenting / active-span stack deferred to T2.
- **T2 ✅ DONE (`2432d6f`).** Scoped `with_span(name, body) -> A` (Class-2 `ctx.call`, **abort-safe**:
  a panic in the body still ends the span with an error status and re-propagates — proven by a
  conformance test) + implicit parenting via a per-run **`ExtState` active-span stack** (the host
  stays a pure factory, per T0), so `span`/`with_span`/`current_context` migrate onto the `NativeCtx`
  seam while the `Span` methods stay plain. `Span` gains `add_event`/`record_error` (status via
  `record_error`), all chaining. **`current_context() -> traceparent` pulled forward from T3** so
  nesting is observable in-language — a conformance test shows an inner span shares its parent's
  trace id but has a distinct span id. *Gate:* conformance differential over 4 telemetry `.noe`
  (basic/nesting/abort + 2 diagnostics) + corpus + leak-0; a stdlib unit test drives the plain `Span`
  methods against the sandbox recorder; 84 stdlib tests + clippy green.
- **T3 ✅ DONE (`e5eb6fe`).** W3C propagation completed. `span_from(name, traceparent)` — the
  **extract** side: parse an inbound `traceparent` (malformed → no-parent → new root, the
  forgiving-reader rule) and continue that remote trace; ctx-dispatched (it starts a span). Plus
  `Span.context() -> str` — the **inject** side for a *held* span (serialize a specific span, not just
  the active one), complementing T2's `current_context()`. **Cross-isolate propagation needed no new
  machinery**: a `traceparent` is a plain string, so it rides a channel message as-is — a conformance
  test sends a producer isolate's `span.context()` over a channel and asserts the consumer's
  `span_from` continued span shares the trace id. *Gate:* conformance differential over 6 telemetry
  `.noe` (both backends agree) + leak-0; a stdlib unit test round-trips `Span.context()` through
  `TraceContext::parse`; 85 stdlib + clippy green. **En route, found & fixed a pre-existing startup-
  cache collision** (`9e4e01e`): the key sorted entry + siblings together, so two programs in one
  directory (same sibling set) shared a key and the second `noeta run` served the first's bytecode —
  now the entry folds through a distinct `KeyBuilder::entry` slot (key scheme v1→v2, regression test).
- **T4 ✅ DONE (`6e9884b`).** Server auto-instrumentation — the headline. `serve.rs`'s per-connection
  handler call is wrapped in a **SERVER-kind span**: parent extracted from the inbound `traceparent`
  (continues the client's trace; absent/malformed → root), named `"{method} {route}"` (query stripped
  for low cardinality), OTel HTTP semantic attributes (`http.request.method`/`url.path`/
  `http.response.status_code`), timed across the handler, error status only on `5xx`. The span rides
  each in-flight handler and ends at reply (incl. the abort→500 path). **Gated on a new
  `Telemetry::tel_enabled()`** so an unconfigured `noeta serve` does zero span work per request
  (perf-gate #1 by construction); `OTEL_SDK_DISABLED=true` honored as the standard opt-out. *Verify:*
  naming/extraction/status split into pure helpers + unit-tested (traceparent extract, 5xx rule); a
  new end-to-end oracle (`crates/noeta-conformance/tests/telemetry_serve.rs`) runs a served program on
  a sandbox host with a **span sink** (`SandboxHost::set_span_sink`, the write-only-span introspection
  path) and asserts the 5 emitted SERVER spans; http_server differential unchanged; 88 stdlib + full
  corpus differential/leak + clippy green; runtime builds telemetry on/off. **Handler-internal span
  nesting deferred to T5** (needs per-task async context — decided with the user).
- **T5 — deeper auto-instrumentation + per-task async context.** The **active-span context becomes
  per-task** (each task/future carries its own stack, not one global LIFO), which is the prerequisite
  for two things: (a) **handler-internal spans nesting under the T4 SERVER span** into one trace per
  request (deferred here from T4 by design — the serve loop is cooperatively concurrent, so a global
  LIFO mis-parents across `await`s); (b) **async task spans** + isolate-message context propagation
  (in). Plus **reactive-flush spans behind an opt-in flag** (per-signal-flush tracing is too noisy by
  default).
- **T6 — docs + close-out.** `docs/Observability.md` (or `Telemetry.md`) — the SDK surface, config,
  the profiler-vs-OTEL split, the differential/bundle stances; CLI/sidebar/roadmap cross-links;
  roadmap tick; memory.

## Perf gate

Two pinned A/B measurements on a quiet box (the project bench rule):
1. **Unconfigured overhead** — a hot loop calling `with_span` with no endpoint set must be ~free
   (null sink short-circuits): within noise vs. the same loop with the calls removed.
2. **`noeta serve` throughput** — tracing-off unchanged vs. `main`; tracing-on within a stated
   budget (batched export is off the request path). Report both like the higher-order-abi gates.

## Guardrails

The differential/leak/conformance oracles make the language-surface slices tedious-not-risky:
program output cannot drift (telemetry is write-only), and the leak oracle proves `Span`/arena
discipline exactly as it did for reactive. The one genuinely new-code risk is the hand-written
OTLP/JSON exporter — covered by a round-trip unit test (span → JSON → parse) against the OTLP schema
plus a real-collector smoke test, both real-host-only (outside the differential).

## Deferred (own follow-on arcs)

- **Metrics + logs** — counters/histograms/gauges and log-record export (larger surface; the
  exporter and config seam built here are reused).
- **Sampling policy** — head/tail sampling beyond always-on / parent-based.
- **Finishing footprint-selected linking** (`aot-dce`) so unused telemetry is zero-cost in AOT
  binaries — telemetry is the strongest motivating case but the mechanism is that arc's to build.
