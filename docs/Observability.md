# Observability

Production [OpenTelemetry](https://opentelemetry.io/) for Noeta programs — all **three signals**:

| Signal | Module | What it is |
|---|---|---|
| **Traces** | `std.tracing` | *Spans* wrapping a unit of work (a request, a task, an effect), linked into a **trace** that follows the work across `await`s, channels, and isolates. |
| **Logs** | `std.log` | Structured, exported log records — **auto-correlated** to the span they were emitted in (trace + span id), not `print`. |
| **Metrics** | `std.metrics` | Instruments (counter / up-down counter / histogram / gauge) **aggregated** host-side into time series per attribute set, exported periodically. |

All three share one seam: nothing is emitted until you point Noeta at a collector, and each is a *write-only side effect* (it never re-enters program output). "Telemetry" is the umbrella name; the three signals get parallel module names.

## Profiling vs. telemetry — two different tools

Noeta ships two things people call "observability"; they don't overlap:

| | [`noeta profile`](Profiling) | telemetry (this page) |
|---|---|---|
| **Question** | *Where does my program spend time?* | *What happened during this request, across services?* |
| **When** | Dev time, one run, on your machine | Production, continuously, exported to a collector |
| **Output** | A flamegraph / hot-function table | Traces/logs/metrics in Jaeger, Tempo, Honeycomb, … |
| **Cost model** | Instruments every op (dev only) | ~Free unless you configure an endpoint |

Reach for the profiler to make one run faster; reach for telemetry to understand a live system.

## Five-minute quick start: see a trace

Any OTLP collector works — Jaeger, Tempo, Honeycomb, an OpenTelemetry Collector. Jaeger's all-in-one image is the fastest to stand up locally:

```sh
docker run --rm -p 16686:16686 -p 4318:4318 jaegertracing/jaeger:latest
```

Write a program with a span (this is the page's tracing example, trimmed):

```noeta check
use std.{tracing}

fn handle_order(id: int): void {
    tracing.with_span("handle_order", fn(): void {
        span = tracing.span("db.lookup")
        span.set_attribute("order.id", id)
        span.end()
    })
}

handle_order(7)
```

Run it with the endpoint set — the env var is the on-switch, there is no flag:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 OTEL_SERVICE_NAME=orders noeta run app.noe
```

Open Jaeger's UI at [http://localhost:16686](http://localhost:16686), pick the `orders` service, and hit *Find Traces*: one trace, `handle_order` with `db.lookup` nested under it. Unset the env var and the same program emits nothing — telemetry is opt-in, as the next section explains.

## Telemetry is opt-in

Nothing is emitted until you point Noeta at a collector. The switch is the **standard OTLP endpoint env var** — there is no `--otel` flag, by design (the env vars *are* the configuration, matching every other OpenTelemetry SDK):

| State | How | Result |
|---|---|---|
| **Off** (default) | `OTEL_EXPORTER_OTLP_ENDPOINT` unset | Nothing emitted, zero cost, no connection attempts. |
| **On** | `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318` | All three signals export; auto-instrumentation active. |
| **Force-off** | `OTEL_SDK_DISABLED=true` | Null sink even with an endpoint set — the opt-out. |
| **One signal off** | `OTEL_TRACES_EXPORTER=none` (or `_LOGS_` / `_METRICS_`) | Disable that one signal while the others export. |

> **Deliberate deviation from the OTel default.** The spec defaults the endpoint to `http://localhost:4318`; Noeta treats *unset* as *off* instead. For a language runtime that's the right call — otherwise every `noeta run` would fire connection attempts at a collector that usually isn't there. You opt in by naming an endpoint.

### Configuration

Read through the standard environment variables (via the `Env` capability):

| Variable | Purpose |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base OTLP/HTTP endpoint. The on-switch. Each signal POSTs to `…/v1/traces`, `…/v1/logs`, `…/v1/metrics`. |
| `OTEL_EXPORTER_OTLP_{TRACES,LOGS,METRICS}_ENDPOINT` | Override one signal's endpoint specifically (used verbatim). |
| `OTEL_{TRACES,LOGS,METRICS}_EXPORTER` | `none` disables that one signal (endpoint presence stays the master switch). |
| `OTEL_EXPORTER_OTLP_HEADERS` | `k1=v1,k2=v2` — e.g. an auth token for a hosted collector. Shared across signals. |
| `OTEL_SERVICE_NAME` | The `service.name` resource attribute (default `noeta`). |
| `OTEL_METRIC_EXPORT_INTERVAL` | Metrics periodic-export interval in ms (default `60000`). |
| `OTEL_METRIC_CARDINALITY_LIMIT` | How many attribute sets each metric aggregates separately (default `2000`). See [Cardinality](#cardinality-and-the-overflow-series). |
| `OTEL_SDK_DISABLED` | `true` forces the null sink for all signals. |
| `NOETA_TRACE_REACTIVE` | `1`/`true` additionally traces reactive flushes and view diffs (opt-in — too noisy for default-on; see below). |

Export is **OTLP over HTTP/JSON**. Traces and logs batch and flush on a threshold or at teardown; metrics aggregate host-side and export on a periodic reader (plus a final flush at teardown).

## Tracing (`std.tracing`)

`use std.{tracing}`. The primary API is the **scoped** span; the manual form is the escape hatch.

| Function | Signature | Notes |
|---|---|---|
| `tracing.with_span` | `with_span(name: string, body: () -> A) -> A` | **Scoped** span: starts, runs `body` as its active parent, ends on exit — **even if `body` aborts** (records an error status, re-propagates). An **async** `body` is tracked to completion (see below). Returns `body`'s value. |
| `tracing.span` | `span(name: string) -> Span` | The lower-level form: start a span parented on the current active one; **you** call `.end()`. Use it when a span outlives a lexical scope. |
| `tracing.current_context` | `current_context() -> string` | The active span's W3C `traceparent` — the **inject** side of manual propagation. Empty when no span is active. |
| `tracing.span_from` | `span_from(name: string, traceparent: string) -> Span` | The **extract** side: start a span continuing a *remote* trace parsed from an inbound `traceparent` (a malformed header → a fresh root). |

### Annotating the span you are *in*

The four functions above all *create* a span or read its context. These four annotate the span that is already **active** — the one you are inside, which no handle names:

| Function | Signature | Notes |
|---|---|---|
| `tracing.set_attribute` | `set_attribute(key: string, value: string\|int\|float\|bool) -> bool` | Set an attribute on the active span. A non-scalar value is a **compile-time** type error. |
| `tracing.add_event` | `add_event(name: string) -> bool` | A timestamped event on the active span. |
| `tracing.add_event_with` | `add_event_with(name: string, attrs: Map<string, string\|int\|float\|bool>) -> bool` | An event carrying its own attributes. |
| `tracing.record_error` | `record_error(message: string) -> bool` | Set the active span's status to error with `message`. |

These are the same mutations the [`Span` handle](#the-span-handle) offers, under the same names, applied to a different receiver. Reach for them whenever something merely *happened* during the current unit of work:

```noeta check
use std.{tracing}

fn run_guardrail(guard: string, reason: string): void {
    tracing.with_span("run", fn(): void {
        // … evaluate the policy …
        tracing.set_attribute("guardrail.verdict", "deny")
        tracing.add_event_with("guardrail.denied", {"guard": guard, "reason": reason})
    })
}
```

Without them, the only way to record that from inside a body was to open a **short child span** — a span per event where an annotation belongs, which inflates trace volume and buries the signal you were trying to record.

**Event or attribute?** An attribute describes the *span* — one value per key, and setting it twice overwrites. An event describes a *moment* — events accumulate, each with its own attribute set. So a fact you may record several times in one span (a verdict per guard, a retry per attempt) belongs in `add_event_with`; a property of the whole operation (the route, the tenant, the final verdict) belongs in `set_attribute`.

They reach the active span at every depth, including spans you did not open. Inside nested `with_span`s the annotation targets the **innermost** span, and the outer one becomes active again when the inner body returns. Inside a request handler it targets the **auto-instrumented SERVER span** — so a handler adds its own attributes to the request's own span, alongside `http.request.method` and `url.path`, with no handle and no child span:

```noeta check
use std.http.server
use std.http.{Request, Response}
use std.{tracing}

fn fetch(req: Request): Response {
    tracing.set_attribute("tenant.tier", "pro")   // rides THIS request's SERVER span
    return server.response(200, "ok")
}
```

**The `bool` is the "no active span" answer.** Each returns whether a live active span received the annotation. It is `false` at top level, and also `false` on a strand that has only been *seeded* by [automatic propagation](#automatic-instrumentation) — a channel message carries a parent context, but the span that context names lives elsewhere and cannot be mutated from here, so open a span over it first. The value exists so that case is visible rather than silent, which matters most for the call whose failure you can least afford to swallow:

```noeta check
use std.{log}
use std.{tracing}

fn report(message: string): void {
    if !tracing.record_error(message) {
        log.error(message)      // no span to carry it — do not lose the error
    }
}
```

> **Why free functions, and not `tracing.current_span(): ?Span`?**
>A `?Span` is the obvious spelling and composes with everything, but it hands the caller a handle carrying **`.end()`** for a span they did not open. A `with_span` body could then end the span `with_span` is itself about to end; a handler could end the SERVER span out from under `server.serve`, corrupting a trace the program never opted into and leaving later child spans parented on a span that already closed. A free function cannot express that hazard — there is no handle to misuse. A restricted handle type (a `Span` minus `end`) would also close the hole, but at the cost of a second span-shaped type that no helper taking a `Span` could accept.
>The deciding argument is that the active span is an **ambient** value, and `std.log` in this same subsystem already treats it as one: `log.info(...)` correlates to the active span with no handle whatsoever. An ambient value's whole point is that you never thread it — a helper three calls deep annotates the current span by calling `tracing.add_event` itself, exactly as it logs. Materializing it into a handle would re-introduce the threading *and* the ownership question. Holding a handle stays the right model for a span **you** opened, which is what `tracing.span(name)` is for.
>The names deliberately mirror the `Span` methods rather than inventing shorter ones (`annotate`, `event`), so one operation keeps one name and someone who knows `span.add_event` finds `tracing.add_event` where they go looking for it.

### The `Span` handle

A `Span` is a mutable handle to one live span (like a file handle — no auto-close). Its mutators **chain**; `end` finalizes. This is the surface for a span **you** opened — to annotate the span you are merely *inside*, use the [free functions above](#annotating-the-span-you-are-in) instead.

| Method | Signature | |
|---|---|---|
| `set_attribute` | `set_attribute(key: string, value: string\|int\|float\|bool) -> Span` | A non-scalar value is a **compile-time** type error. |
| `add_event` | `add_event(name: string) -> Span` | A timestamped event on the span. |
| `add_event_with` | `add_event_with(name: string, attrs: Map<string, string\|int\|float\|bool>) -> Span` | An event carrying its own attributes. |
| `record_error` | `record_error(message: string) -> Span` | Sets the span's status to error with `message`. |
| `context` | `context() -> string` | This span's own `traceparent` — inject a *specific* held span (vs. `current_context`'s active one). |
| `end` | `end() -> void` | Finalize; the span is exported. |

```noeta check
use std.{tracing}

fn handle_order(id: int): void {
    tracing.with_span("handle_order", fn(): void {
        span = tracing.span("db.lookup")
        span.set_attribute("db.system", "postgres").set_attribute("order.id", id)
        // … query …
        span.end()
    })
}
```

> `Span` is a namespaced extern type, not a reserved name — you may declare your own `Span`. A clash only arises if you also `use std.tracing.Span` in the same scope, which is an import conflict (**E0020**). (E0049 is reserved for the checker-native generics `Iterator`/`Future`/`Sender`/`Receiver`.)

## Logs (`std.log`)

`use std.{log}`. OTel **log records** — structured, exported log lines that are **auto-correlated to the active span**: a log emitted inside a `with_span` (or under a server request) carries that span's trace and span id automatically, so your logs and traces stitch together in the collector with zero threading. A top-level log carries no correlation. These records go to the OTLP sink only — `std.log` is *not* a `print`/stdout bridge.

| Function | Signature | Notes |
|---|---|---|
| `log.info` / `debug` / `warn` / `error` | `info(message: string) -> void` | The per-level conveniences. |
| `log.log` | `log(severity: string, message: string) -> void` | Generic form; severity parsed case-insensitively (unknown → `info`), reaching `trace`/`fatal` too. |
| `log.*_with` | `info_with(message: string, attrs: Map<string, string\|int\|float\|bool>) -> void` | Structured attributes (`log_with`/`debug_with`/…). A non-scalar attribute value is a **compile-time** error. |

```noeta check
use std.{log}
use std.{tracing}

tracing.with_span("handle_order", fn(): void {
    log.info("order received")                                   // carries the span's trace/span id
    log.warn_with("stock low", {"sku": "A-1", "left": 3})        // + structured attributes
})
```

Gated on the logs signal being enabled — a `log.info(...)` in a hot loop is free when no logs endpoint is configured.

## Metrics (`std.metrics`)

`use std.{metrics}`. OTel **instruments** — long-lived, host-owned, **aggregated** into time series per attribute set. A constructor is *get-or-create by name* (idempotent), returning the instrument's handle; its methods record a measurement, aggregated host-side and exported on a periodic reader.

| Constructor | Handle | Aggregation |
|---|---|---|
| `metrics.counter(name)` | `Counter` | Monotonic sum (only goes up); `.add(n)`. |
| `metrics.up_down_counter(name)` | `Counter` | Non-monotonic sum (up and down); `.add(n)`. |
| `metrics.histogram(name)` | `Histogram` | Distribution over the OTel default explicit buckets; `.record(v)`. |
| `metrics.gauge(name)` | `Gauge` | Last-value sample; `.record(v)`. |

Counters record with `.add(n)`; histograms and gauges with `.record(v)`. The `.add_with(n, attrs)` / `.record_with(v, attrs)` forms attach a `Map<string, string|int|float|bool>` of attributes (each distinct set is its own series).

```noeta check
use std.{metrics}

requests = metrics.counter("http.requests")
latency  = metrics.histogram("db.query.duration")

requests.add_with(1, {"route": "/orders", "status": 200})
latency.record(4.2)
```

> **`Counter`/`Histogram`/`Gauge` are namespaced types** under `std.metrics` — `use`-imported like any extern type, so they coexist with a user's own `Counter`. You only need `use std.metrics.{Counter, …}` when you name one in an annotation; the constructors return them regardless.

Gated on the metrics signal being enabled — a `counter.add(...)` in a hot loop is free when no metrics endpoint is configured.

### Cardinality and the overflow series

Every distinct attribute set is a separate stored series, held host-side for the life of the process. So attribute *values* want to be low-cardinality — a route template, a status class, a tenant tier — and a request id, a user id, a raw path with ids in it or a timestamp is a value that never repeats, which means a new series for every measurement.

Each instrument therefore aggregates at most **2000** attribute sets separately. Measurements whose attribute set arrives after that are not dropped: they fold into one synthetic series marked `otel.metric.overflow=true`, so the instrument's total stays exact and only its breakdown stops. That is what you are looking at when a data point shows up in your collector carrying that attribute and nothing else — the metric it belongs to has more attribute sets than it is allowed to keep apart, and the number under `otel.metric.overflow` is everything that could not be told apart.

The limit is **per instrument**, so one counter carrying a bad attribute does not cost the rest of the program its detail. The sets an instrument saw *before* it filled up keep their own series and keep accumulating; only sets first seen afterwards fold.

Raise or lower it with `OTEL_METRIC_CARDINALITY_LIMIT`:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 OTEL_METRIC_CARDINALITY_LIMIT=10000 noeta run app.noe
```

The value is a count of attribute sets, and it applies to every instrument. Anything that is not a positive whole number — a typo, an empty value, `0` — leaves the default in place rather than being read literally, because a limit of zero would fold every attribute set in the program into one bucket and hide every breakdown you have.

Reaching the overflow series is a signal about the *instrument*, not a budget to raise: the fix is almost always to take the high-cardinality value out of the metric and put it on a [span attribute](#annotating-the-span-you-are-in) or a [log record](#logs-stdlog) instead, where one value per event is the point.

## Automatic instrumentation

Once telemetry is configured, the runtime opens spans for you at the boundaries that matter — no code changes:

- **Server requests.** Every connection the bundled server (`server.serve`, from `std.http.server`) accepts is wrapped in a **SERVER** span named `"{method} {route}"`, parented on the inbound `traceparent` (so it continues the caller's trace), carrying `http.request.method` / `url.path` / `http.response.status_code`, timed across the handler, and marked an error only on a `5xx`. Your handler's own spans (and logs) nest *under* it — one connected trace per request. The handler can also annotate that span directly with [`tracing.set_attribute` / `add_event` / `record_error`](#annotating-the-span-you-are-in), which is usually what you want for a per-request fact: no handle exists for the SERVER span, and a child span per fact is a span where an annotation belongs. The server still ends the span itself — a handler cannot. (See [Concurrency](Concurrency) for the server itself.)
- **Server metrics.** The same requests also record the `http.server.request.duration` histogram (seconds, keyed by method / route / status) and maintain an `http.server.active_requests` up/down counter — the metrics twin of the SERVER span, no code changes.
- **Async work.** A `with_span` over an `async` body follows the future: the span stays active across the body's suspensions and ends when the work *completes*, not when the coroutine is constructed. Spans the body creates after an `await` still nest correctly.
- **Channels & isolates.** Sending on a channel attaches the sender's trace context to the message; the receiver — and a spawned `isolate` — is seeded with it automatically, so the far side continues the same trace with no manual threading. (The message's *type* is untouched.)
- **Reactive propagation** (opt-in: set `NOETA_TRACE_REACTIVE=1` alongside the endpoint). Every non-empty reactive flush becomes a `reactive.flush` span carrying `reactive.effects` (effect bodies run) and `reactive.changed` (distinct nodes whose value changed); a LiveView `view.diff` becomes a `view.diff` span carrying `view.dirty` (bindings inspected) vs `view.pushed` (bindings actually sent). The flush span is the active parent while effect bodies run, so their own spans nest under it — a click's trace reads *event → signal set → flush (N effects) → diff (K pushed)*. Opt-in because per-set flush tracing is far too noisy for default-on; when off, the cost is one cached-boolean branch per flush.

### Manual propagation (interop)

Auto-propagation covers Noeta's own boundaries. To bridge a boundary Noeta doesn't own — an outbound HTTP call to another service, a queue — thread the `traceparent` yourself:

```noeta check
use std.{tracing}

// Inject on the way out:
tp = tracing.current_context()          // "00-<trace>-<span>-01"
// … send `tp` as the `traceparent` header / message field …

// Extract on the way in:
span = tracing.span_from("consume", inbound_traceparent)
```

## When production breaks

An aborting request is visible in the trace: the auto-instrumented SERVER span (`"{method} {route}"`) answers a `500`, so it ends carrying `http.response.status_code: 500` and an **error status** (`HTTP 500` — only a `5xx` marks the span an error; a `4xx` is the client's fault). Any `with_span` the handler was inside also ends with an error status (`span body aborted`), so the innermost error span points at the failing operation. From there the path is local: use the span's attributes (route, method, your own `set_attribute`s) to reproduce the request on your machine, then step through it under [Debugging](Debugging) — the debugger is launch-only, so you re-run the program rather than attach to the live one. If the symptom is slowness rather than an abort, take the reproduction to [Profiling](Profiling) instead.

## Design notes

Telemetry is a **write-only side effect**: a span, a log, a metric never re-enters program output, so it can't change what a program computes. The active-span stack is **task-local** — each cooperative task and isolate carries its own, and a spawned task inherits a *snapshot* of its spawner's taken at spawn time — so interleaved work can't cross-parent and logs correlate to the right span. A snapshot, not a view: a task keeps reading the scope it was launched under even after its spawner has moved into a different span, and it can never reach a sibling's live one. Everything that reads "the current span" — `current_context`, `std.log`'s correlation, and the active-span annotators — reads that same per-strand stack, so they always agree about which span you are in. Metric aggregation is host-side, exported on a periodic reader plus a final flush. A program that never imports a telemetry module and never sets an endpoint pays nothing at runtime, and the exporter is a lean OTLP/HTTP-JSON writer behind a default-on `telemetry` build feature. Both backends are held to byte-identical telemetry by parity oracles — see [The Virtual Machine](The-Virtual-Machine) for that internals story.

## See also

- [Concurrency](Concurrency) — `server.serve`, channels, isolates (the boundaries auto-instrumentation traces).
- [Profiling](Profiling) — the dev-time flamegraph tool (the *other* observability half).
- [std.tracing](std-tracing), [std.log](std-log), [std.metrics](std-metrics) — the observability modules in the generated stdlib reference.
- [Native Extensions](Native-Extensions) — the `Tracing`/`Logging`/`Metrics` Host capabilities and the higher-order seam the SDKs dispatch through.
