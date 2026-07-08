# Observability (`std.telemetry`)

Production **distributed tracing** for Noeta programs, emitted as [OpenTelemetry](https://opentelemetry.io/).
A *span* wraps a unit of work — a request, a task, an effect — and carries a name, timing, attributes,
events, a status, and a link to its parent. Spans from one logical operation share a **trace**, and
trace context propagates across `await`s, channels, and isolate boundaries so a request that fans out
into concurrent work still reads as one connected trace.

> **Tracing, not metrics/logs.** This is the tracing signal. Metrics (counters/histograms) and log
> export are a natural sibling but a separate, larger surface — not built yet.

## Profiling vs. tracing — two different tools

Noeta ships two things people call "observability"; they don't overlap:

| | [`noeta profile`](Profiling) | `std.telemetry` (this page) |
|---|---|---|
| **Question** | *Where does my program spend time?* | *What happened during this request, across services?* |
| **When** | Dev time, one run, on your machine | Production, continuously, exported to a collector |
| **Output** | A flamegraph / hot-function table | Spans in Jaeger, Tempo, Honeycomb, … |
| **Cost model** | Instruments every op (dev only) | ~Free unless you configure an endpoint |

Reach for the profiler to make one run faster; reach for telemetry to understand a live system.

## Telemetry is opt-in

Nothing is emitted until you point Noeta at a collector. The switch is the **standard OTLP endpoint
env var** — there is no `--otel` flag, by design (the env vars *are* the configuration, matching
every other OpenTelemetry SDK):

| State | How | Result |
|---|---|---|
| **Off** (default) | `OTEL_EXPORTER_OTLP_ENDPOINT` unset | Zero spans, zero cost, no connection attempts. |
| **On** | `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318` | Spans export; auto-instrumentation active. |
| **Force-off** | `OTEL_SDK_DISABLED=true` | Null sink even with an endpoint set — the opt-out. |

> **Deliberate deviation from the OTel default.** The spec defaults the endpoint to
> `http://localhost:4318`; Noeta treats *unset* as *off* instead. For a language runtime that's the
> right call — otherwise every `noeta run` would fire connection attempts at a collector that usually
> isn't there. You opt in by naming an endpoint.

### Configuration

Read through the standard environment variables (via the `Env` capability):

| Variable | Purpose |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base OTLP/HTTP endpoint (spans POST to `…/v1/traces`). The on-switch. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Overrides the traces endpoint specifically (used verbatim). |
| `OTEL_EXPORTER_OTLP_HEADERS` | `k1=v1,k2=v2` — e.g. an auth token for a hosted collector. |
| `OTEL_SERVICE_NAME` | The `service.name` on exported spans (default `noeta`). |
| `OTEL_SDK_DISABLED` | `true` forces the null sink. |

Export is **OTLP over HTTP/JSON**, batched and flushed on a threshold or at program teardown.

## The SDK surface

`use std.{telemetry}`. The primary API is the **scoped** span; the manual form is the escape hatch.

| Function | Signature | Notes |
|---|---|---|
| `telemetry.with_span` | `with_span(name: string, body: () -> A) -> A` | **Scoped** span: starts, runs `body` as its active parent, ends on exit — **even if `body` aborts** (records an error status, re-propagates). An **async** `body` is tracked to completion (see below). Returns `body`'s value. |
| `telemetry.span` | `span(name: string) -> Span` | The lower-level form: start a span parented on the current active one; **you** call `.end()`. Use it when a span outlives a lexical scope. |
| `telemetry.current_context` | `current_context() -> string` | The active span's W3C `traceparent` — the **inject** side of manual propagation. Empty when no span is active. |
| `telemetry.span_from` | `span_from(name: string, traceparent: string) -> Span` | The **extract** side: start a span continuing a *remote* trace parsed from an inbound `traceparent` (a malformed header → a fresh root). |

### The `Span` handle

A `Span` is a mutable handle to one live span (like a file handle — no auto-close). Its mutators
**chain**; `end` finalizes.

| Method | Signature | |
|---|---|---|
| `set_attribute` | `set_attribute(key: string, value: string\|int\|float\|bool) -> Span` | A non-scalar value is a **compile-time** type error. |
| `add_event` | `add_event(name: string) -> Span` | A timestamped event on the span. |
| `record_error` | `record_error(message: string) -> Span` | Sets the span's status to error with `message`. |
| `context` | `context() -> string` | This span's own `traceparent` — inject a *specific* held span (vs. `current_context`'s active one). |
| `end` | `end() -> void` | Finalize; the span is exported. |

```noeta ignore
use std.{telemetry}

fn handle_order(id: int): void {
    telemetry.with_span("handle_order", fn(): void {
        span = telemetry.span("db.lookup")
        span.set_attribute("db.system", "postgres").set_attribute("order.id", id)
        // … query …
        span.end()
    })
}
```

> `Span` is a reserved type name — declaring your own `Span` is a compile error (**E0049**).

## Automatic instrumentation

Once telemetry is configured, the runtime opens spans for you at the boundaries that matter — no code
changes:

- **Server requests.** Every connection the bundled server (`server.serve`, from `std.http.server`)
  accepts is wrapped in a **SERVER** span named
  `"{method} {route}"`, parented on the inbound `traceparent` (so it continues the caller's trace),
  carrying `http.request.method` / `url.path` / `http.response.status_code`, timed across the handler,
  and marked an error only on a `5xx`. Your handler's own spans nest *under* it — one connected trace
  per request. (See [Concurrency](Concurrency) for the server itself.)
- **Async work.** A `with_span` over an `async` body follows the future: the span stays active across
  the body's suspensions and ends when the work *completes*, not when the coroutine is constructed.
  Spans the body creates after an `await` still nest correctly.
- **Channels & isolates.** Sending on a channel attaches the sender's trace context to the message;
  the receiver — and a spawned `isolate` — is seeded with it automatically, so the far side continues
  the same trace with no manual threading. (The message's *type* is untouched.)

### Manual propagation (interop)

Auto-propagation covers Noeta's own boundaries. To bridge a boundary Noeta doesn't own — an outbound
HTTP call to another service, a queue — thread the `traceparent` yourself:

```noeta ignore
// Inject on the way out:
tp = telemetry.current_context()          // "00-<trace>-<span>-01"
// … send `tp` as the `traceparent` header / message field …

// Extract on the way in:
span = telemetry.span_from("consume", inbound_traceparent)
```

## Design notes

- **Write-only, never differential-tested.** A span never re-enters program output, so both Noeta
  backends produce identical results regardless of what they emit — telemetry is real-host-only, like
  the network. A deterministic in-memory recorder backs the sandbox purely so conformance can assert
  on emitted spans (both backends are held to *byte-identical* span trees by a parity oracle).
- **Per-task context.** The active-span stack is **task-local**: each cooperative task (and each
  isolate) carries its own, so interleaved work can't cross-parent. A spawned task inherits a snapshot
  of its spawner's context.
- **Bundle cost.** A program that never imports `std.telemetry` and never sets an endpoint pays
  nothing at runtime. The exporter is a hand-rolled OTLP/HTTP-JSON writer over the `reqwest` +
  `serde_json` already in the build — deliberately **not** `opentelemetry-otlp` (which drags
  `tonic`/`prost`) — and sits behind a default-on `telemetry` build feature a minimal CLI can drop.

## See also

- [Concurrency](Concurrency) — `server.serve`, channels, isolates (the boundaries auto-instrumentation traces).
- [Profiling](Profiling) — the dev-time flamegraph tool (the *other* observability half).
- [Standard-Library Modules](Standard-Library-Modules#telemetry) — the module in the stdlib reference.
- [Native Extensions](Native-Extensions) — the `Telemetry` Host capability and the higher-order seam the SDK dispatches through.
