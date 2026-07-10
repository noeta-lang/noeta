# Native OTEL — metrics + logs signals (plan)

**Follow-on to the native-otel tracing arc** (`plans/native-otel/README.md`, merged to local `main`
`7ecc935`). Tracing shipped the whole scaffold this arc reuses: the `Telemetry` Host capability (9th),
the `std.telemetry` SDK, the hand-rolled OTLP/HTTP-JSON exporter, per-task task-local context, and the
**byte-identical sandbox-parity oracle** for write-only side effects. Metrics and logs are the two
remaining OpenTelemetry signals; both are also write-only, so the same oracle strategy transfers
wholesale.

**Branch:** `native-otel-metrics-logs` (off local `main`). **Worktree:** `.claude/worktrees/…`.

## What we're adding

OTel has three signals. Tracing is done. The other two:

| Signal | Data model | New per-op cost | Export cadence | New risk |
|---|---|---|---|---|
| **Logs** | `LogRecord`: time, severity, body, attrs, **+ trace/span id of the active span** | emit-and-forget | batch → threshold/teardown (same as spans) | low — reuses everything |
| **Metrics** | Instruments (Counter/UpDownCounter/Histogram/Gauge) → **aggregated** time series per attribute-set | `add`/`record` into host-side aggregation | **periodic** collection (default 60s) + teardown | high — stateful aggregation + a periodic reader with no guaranteed background timer |

**Logs are cheap and high-value** (structured records auto-correlated to the trace that produced them —
the killer feature, and it's just a read of the task-local context stack tracing already maintains).
**Metrics are the larger surface** (aggregation state, cardinality, temporality, and a periodic export
loop). So the arc does **logs first** to prove the multi-signal scaffolding (a second OTLP endpoint, a
second sandbox recorder, a non-span parity oracle) on the small signal, then metrics on top of it.

## Architecture — same three seams as tracing

Everything extends structures the tracing arc already built; no new crate. The `Host` union gains two
sub-trait arms (the tracing capability is renamed and joined by `Metrics` + `Logging` — see below).

### 1. Host capability — three sub-traits (`noeta-native/src/telemetry.rs`)

**Decision — split into three capability sub-traits `Tracing` / `Metrics` / `Logging`** (today's
`Telemetry` trait is **renamed `Tracing`**; its methods are unchanged). The `Host` union grows from
`… + crate::Telemetry` to `… + crate::Tracing + crate::Metrics + crate::Logging` (three arms + a wider
blanket impl); each backend writes three `impl` blocks. **Runtime cost: none** — a `dyn Host` has one
vtable and supertrait methods fold into it, so a call is one indirection regardless of which sub-trait
declared it; and the per-op hot paths never make the virtual call anyway — they gate on cached bools
(`tel_on` today, `tel_logs_enabled`/`tel_metrics_enabled` here), the T5d pattern (a per-send virtual
`tel_enabled()` cost +3.2%; the cached bool is flat). The split's only cost is ~30 lines of boilerplate;
the benefit is one file + one ABI group per signal and per-signal generic bounds. The shared host state
objects (`RealTelemetry` / `TelRecorder`) just gain metric/log fields — one struct still backs all three
`impl`s per host.

New neutral ABI data (plain `Send`, no OTel-crate types cross the seam — the arc's invariant):

```rust
// logs
pub enum Severity { Trace, Debug, Info, Warn, Error, Fatal }   // → OTel severity number (1..24)
pub struct LogRecord {
    pub unix_ms: u64,
    pub severity: Severity,
    pub body: CompactString,
    pub attributes: Vec<(CompactString, AttrValue)>,   // AttrValue already exists
    pub trace_context: Option<TraceContext>,           // active-span correlation, already exists
}

// metrics
pub enum InstrumentKind { Counter, UpDownCounter, Histogram, Gauge }
pub type InstrumentId = u64;
pub enum MetricValue { Int(i64), Float(f64) }
// aggregation lives host-side; these are the *collected* export shapes for the recorder/exporter:
pub struct NumberPoint    { pub attrs: Vec<(CompactString, AttrValue)>, pub value: MetricValue, pub start_unix_ms: u64, pub unix_ms: u64 }
pub struct HistogramPoint { pub attrs: …, pub count: u64, pub sum: f64, pub bounds: Vec<f64>, pub buckets: Vec<u64>, … }
pub struct MetricData {
    pub name: CompactString, pub unit: CompactString, pub kind: InstrumentKind,
    pub temporality: Temporality, pub points: … // sorted deterministically
}
pub enum Temporality { Cumulative, Delta }
```

New trait methods:

```rust
// logs
fn tel_logs_enabled(&self) -> bool;                 // cached like tel_enabled → a per-op hot-path bool
fn log_emit(&mut self, record: LogRecord);

// metrics
fn tel_metrics_enabled(&self) -> bool;
fn metric_get_or_create(&mut self, name: &str, unit: &str, kind: InstrumentKind) -> InstrumentId; // idempotent by name
fn metric_add(&mut self, inst: InstrumentId, value: MetricValue, attrs: Vec<(CompactString, AttrValue)>);    // Counter/UpDownCounter
fn metric_record(&mut self, inst: InstrumentId, value: MetricValue, attrs: Vec<(CompactString, AttrValue)>); // Histogram/Gauge
fn metric_collect(&mut self) -> Vec<MetricData>;    // snapshot the aggregation (sandbox: teardown only; real: periodic + teardown)
```

**Instruments are long-lived and host-owned.** `metric_get_or_create` is idempotent on `name` (OTel
"identical instrument" rule) → the aggregation state persists across calls even though the language-side
`Counter` handle is just an `InstrumentId` (exactly how `Span` wraps a `SpanId`). Aggregation (sum,
histogram bucketing) is **entirely host-side**; the ABI only carries `add`/`record` events in and
collected `MetricData` out. Both host impls aggregate identically, so byte-identical parity holds.

### 2. SDK modules — one per signal (`noeta-stdlib`)

Symmetric with the three traits: three modules, each registered in the registry like `std.telemetry`
is today (the checker auto-derives every signature — zero `noeta-check` edits). **These are std
*modules* in `noeta-stdlib`, not separate workspace crates** (matching `std.http`/`std.crypto`/…).

| Signal | Trait | Module | Status |
|---|---|---|---|
| tracing | `Tracing` | `std.tracing` | **renamed** from `std.telemetry` (surface otherwise unchanged: `with_span`/`span`/`span_from`/`Span`) |
| logs | `Logging` | **`std.log`** | new |
| metrics | `Metrics` | **`std.metrics`** | new |

"Telemetry" stays the **umbrella** name (the plans dir, `docs/Observability.md`); the three *signals*
get parallel trait+module names. The `std.telemetry` → `std.tracing` rename is the arc's opening slice
(P-1 below) — shipped but local-only, small mechanical surface, fully oracle-covered.

**`std.log`** — module functions, ctx-dispatched (they read the active-span context for correlation):

| Fn | Signature | Notes |
|---|---|---|
| `log.log` | `log(severity: string, message: string) -> void` | severity parsed to `Severity`; unknown → `Info` |
| `log.debug`/`info`/`warn`/`error` | `info(message: string) -> void` | conveniences |
| (attrs form) | `info_with(message, attrs: Map<string, string\|int\|float\|bool>) -> void` | structured attributes |

Each pulls `current_parent(ctx)` (the active-span `TraceContext`) into the record — a log inside a span
carries that span's trace/span id automatically, with zero user threading. This is the whole value
proposition and it's a one-line reuse of the tracing context stack.

**`std.metrics`** — instrument constructors return extern handles; instruments' methods marshal to the host:

| Fn | Signature |
|---|---|
| `counter` | `counter(name: string) -> Counter` |
| `up_down_counter` | `up_down_counter(name: string) -> Counter` |
| `histogram` | `histogram(name: string) -> Histogram` |
| `gauge` | `gauge(name: string) -> Gauge` |

`Counter.add(n: int\|float)` / `.add(n, attrs)`, `Histogram.record(v)` / `.record(v, attrs)`,
`Gauge.record(v)` / `.record(v, attrs)`. Instrument constructors are ctx-dispatched (get-or-create
touches host state); `.add`/`.record` are plain dispatch (host-only, like the `Span` mutators).

**New extern types** `Counter`/`Histogram`/`Gauge` (or one `Instrument` type carrying its kind — decide
in M1; separate types give better method-typing and clearer errors, at the cost of three reserved names
and three new **E00xx** diagnostics — next free is **E0050**). Reserve as `Span` is reserved (E0049).

**Attribute maps.** `attrs` is `Map<string, string|int|float|bool>` — `SigType::Map(&Str, &ATTR_VALUE)`
where `ATTR_VALUE` is the existing scalar `SigType::Union`. The signature composes today; the **first
slice must verify the checker accepts a map *literal* with union values at the call site** (`{"route":
"/x", "status": 200}`). Fallback if it doesn't: a small attribute-builder value, or variadic key/value
pairs. Flag, don't assume.

### 3. OTLP exporter (`noeta-runtime/src/telemetry.rs`)

The exporter is traces-only today (`OtlpExporter { traces_endpoint, … }`, `spans_to_json`). Generalize:

- **Phase 0 refactor:** hold a base endpoint + resolved per-signal endpoints (`…/v1/traces`,
  `…/v1/metrics`, `…/v1/logs`) + shared headers/service-name; one `post(url, body)` helper; a shared
  `resource()` builder (the `service.name` block is duplicated per signal in OTLP). **No behavior change
  to traces** — the existing `otlp_json_shape_is_valid` test is the regression guard.
- **Logs:** `logs_to_json` → `ExportLogsServiceRequest` (`resourceLogs → scopeLogs → logRecords[]` with
  `timeUnixNano`, `severityNumber`, `severityText`, `body.stringValue`, `attributes`, and
  `traceId`/`spanId` hex from the record's `trace_context`). Buffer + flush like spans.
- **Metrics:** `metrics_to_json` → `ExportMetricsServiceRequest` (`resourceMetrics → scopeMetrics →
  metrics[]`; each metric is `sum`/`histogram`/`gauge` with `dataPoints[]`, `aggregationTemporality`,
  `isMonotonic` for Counter). The heaviest new JSON mapping — its own round-trip unit test against the
  OTLP schema. Metrics export on a **periodic reader**, not per-op (below).

Per-signal gating from the standard env (`OTEL_TRACES_EXPORTER` / `OTEL_METRICS_EXPORTER` /
`OTEL_LOGS_EXPORTER` = `none` disables one signal; endpoint presence is still the master on-switch). No
new deps — reuses `reqwest` + `serde_json` already compiled; all under the existing `telemetry` feature.

## The three hard decisions (call-outs, with recommendations)

1. **Metrics periodic export vs teardown-only.** A Counter's exported value depends on *when* collection
   fires. A long-running server that never exits is useless if metrics only ship at teardown — so
   metrics genuinely need a **periodic reader**, and the runtime has no guaranteed always-on background
   timer. **Recommendation:** a real-host-only periodic-export thread (spawned by `RealHost` only when
   metrics are enabled, behind the `telemetry` feature; `OTEL_METRIC_EXPORT_INTERVAL`, default 60s),
   joined/final-flushed at teardown. The **sandbox stays teardown-only** (single deterministic cumulative
   snapshot) — wall-time periodicity is nondeterministic and the logical clock doesn't advance with real
   timers, so a periodic sandbox reader could never be oracle-stable anyway. This split keeps the oracle
   deterministic while the real host is actually useful. *(This is the single biggest new-code risk.)*

2. **Oracle determinism for aggregated metrics.** Byte-identical parity requires both backends to emit
   the *same* collected `MetricData` in the *same order*. Two rules: (a) sandbox collects **only at
   teardown** (one cumulative point per series); (b) data points within a metric are emitted **sorted by
   attribute-set key**, not insertion order (a `BTreeMap<AttrKey, Aggregation>` host-side). Histograms
   use OTel's **default explicit bucket boundaries** (config deferred). With those, the `with_span`
   parity oracle (`reference_run_with_host` + a metrics/logs sink on `SandboxHost`) transfers unchanged.

3. **Logs get their own `std.log` module** (decided). No `std.log` exists today — these OTel logs are a
   fresh surface: *exported, trace-correlated* records, not `print`. A bridge that also mirrors records
   to stdout is a separate decision, out of this arc (log records go to the OTLP sink only).

## Progress

- **P-1 ✅** (`01355bf`) rename std.telemetry → std.tracing / trait `Telemetry` → `Tracing`.
- **P0 ✅** (`7c660f4`) three-signal `Host` split (`Tracing`/`Metrics`/`Logging`, eleven-arm union) +
  multi-signal `OtlpExporter` (per-signal endpoints + `OTEL_{SIGNAL}_EXPORTER=none`) + shared
  `resource()`/`otlp_post`; cached `tel_logs_enabled`/`tel_metrics_enabled` on both backends.
- **L0 ✅** (`d9313fc`) logs ABI (`Severity`/`LogRecord`) + `log_emit` + sandbox recorder + `logs_to_json`
  (`v1/logs`) + RealHost buffer/flush.
- **L1 ✅** (`e3949fb`) `std.log` module (`info`/`debug`/`warn`/`error` + generic `log`), ctx-dispatched,
  active-span correlation; logs sink-parity oracle.
- **L1.5 ✅** (`6958d8`) **checker prerequisite** (not in the original plan — verify-first item hit the
  anticipated wall): map literals now absorb an expected `Map<K,V>` value type bidirectionally
  (container literals join closures as deferred call args). Unblocks `Map<string, union>` at the call
  site; the plan's fallback (builder/variadic) was **not** needed. User-approved scope into noeta-check.
- **L2 ✅** (`63e115f`) `std.log` structured attributes (`*_with(msg, attrs)`), `Map<string, union>`
  literal accepted; non-scalar value → E0007. Logs signal COMPLETE end-to-end.
- **M0 ✅** (`af2bcc1`) metrics ABI + shared `MetricStore` aggregator (both backends, byte-identical) +
  `metrics_to_json` (`v1/metrics`); sandbox collects at teardown (Drop→sink).
- **M1 ✅** (`03272b0`) `std.metrics` module + **one `Instrument` extern type** (not three — reserving
  `Counter`/`Gauge`/`Histogram` collides with common user code; only `Instrument` is reserved, E0049).
  `ExtType::deep_marshal` added so `*_with` map args reach dispatch as `NativeValue::Map`. Sink-parity
  oracle. **A follow-on `namespaced-extern-types` arc (separate branch) may later enable idiomatic
  per-kind types** (see that arc's plan).
- **M2 ✅** (`e1aaf93`) real-host periodic-export reader (background thread, `Arc<Mutex<MetricStore>>`,
  `OTEL_METRIC_EXPORT_INTERVAL`, lazy-spawn, shutdown+final-flush on Drop); SDK `add`/`record` gated on
  `tel_metrics_enabled` (free when off). End-to-end reader test vs a loopback collector.
- **M3 ✅** (`17de5cc`) server auto-instrumentation: `http.server.request.duration` histogram +
  `http.server.active_requests` up/down counter per request, rides the SERVER-span hook. Sink oracle.

**Both new signals COMPLETE. Next: Phase D (docs + close-out).**

- **MERGE ✅** merged updated `main` (69 commits: PM Phase 3, kernel-methods × OTEL, p2p P3, and the
  **extern-type-namespacing** refactor). Adopted main's `Extension`/`std_unit!` registration model and
  namespaced `ExtType` (identity `namespace.name`, no global reservation). **Payoff of namespacing:
  the single `Instrument` was replaced with idiomatic namespaced `Counter`/`Histogram`/`Gauge` under
  `std.metrics`** (`use std.metrics.{Counter, …}`; they coexist with a user's own `Counter`). Re-added
  `ExtType.deep_marshal` (survived auto-merge) for the `*_with` map args; L1.5 map-absorption survived
  main's checker refactor. Removed the now-invalid reserved-name diagnostic (extern types aren't
  globally reserved). Full gate re-green post-merge.

## Slices (commit per green slice; full gate = workspace suites + differential + leak + conformance + fmt + clippy)

**Phase 0 — shared scaffolding**
- **P-1** Rename the tracing signal to match the new three-signal split: module `std.telemetry` →
  `std.tracing` (registry string + `.noe` corpus `use std.{telemetry}`→`use std.{tracing}` and
  `telemetry.`→`tracing.` + docs + memory), trait `Telemetry` → `Tracing` (`noeta-native` + the `Host`
  union arm + both backend impls). Pure rename, no behavior change. *Gate:* the whole existing telemetry
  suite green under the new names — differential + sink-parity (8 `.noe`) + stdlib + clippy. One commit,
  before any new-signal code.
- **P0** Generalize `OtlpExporter` to multi-signal (base + per-signal endpoints, `post` helper, shared
  `resource()`); per-signal enable from `OTEL_{SIGNAL}_EXPORTER`; add cached `tel_logs_enabled` /
  `tel_metrics_enabled` bools to both backends (mirroring `tel_on`). No language surface, no new signal
  yet. *Gate:* traces unchanged (existing exporter + sink tests green); both feature-on/off builds.

**Phase L — logs** *(prove multi-signal on the cheap signal)*
- **L0** ABI (`Severity`, `LogRecord`) + `Telemetry::log_emit` + `tel_logs_enabled`; sandbox recorder
  buffers `LogRecord`s (+ a logs sink for the oracle); real host `logs_to_json` + `v1/logs` POST,
  buffer/flush like spans. *Gate:* logs-JSON round-trip unit test; deterministic recorder test; feature
  on/off. No language surface.
- **L1** `std.log` module (`log`/`debug`/`info`/`warn`/`error`) + **active-span correlation**
  (reuse `current_parent`); ctx-dispatched. *Gate:* a sink-parity conformance oracle (`.noe` runs on
  **both** backends, byte-identical `LogRecord`s incl. trace/span id) — a log emitted inside `with_span`
  carries that span's ids; a top-level log has none. Leak-0.
- **L2** Structured attributes (`info_with(msg, attrs)`) — **first exercise of the `Map<string, union>`
  attribute literal**; if the checker rejects it, pivot to the fallback here (§seam). Diagnostics if a
  non-scalar attribute value → compile error. *Gate:* parity oracle over attributed logs + the
  diagnostics corpus entry.

**Phase M — metrics** *(the larger signal, on proven scaffolding)*
- **M0** ABI (`InstrumentKind`/`InstrumentId`/`MetricValue`/`NumberPoint`/`HistogramPoint`/`MetricData`/
  `Temporality`) + trait methods (`metric_get_or_create`/`add`/`record`/`collect`) + `tel_metrics_enabled`.
  Sandbox recorder aggregates (cumulative, `BTreeMap` by attr-key, collect-at-teardown); real host
  aggregates identically. `metrics_to_json` (sum/gauge/histogram) + `v1/metrics` + round-trip unit test.
  Default histogram buckets. *Gate:* metrics-JSON shape test; deterministic aggregation test (same call
  sequence → identical collected points, sorted); feature on/off.
- **M1** `std.metrics` module — `counter`/`up_down_counter`/`histogram`/`gauge`
  constructors (ctx, get-or-create) + `Counter`/`Histogram`/`Gauge` extern types with `.add`/`.record`
  (plain dispatch); reserve the type name(s) → new **E0050+**; attributes reuse L2's map form. *Gate:* a
  sink-parity oracle collecting at teardown — byte-identical `MetricData` across backends (counter sum,
  histogram buckets, per-attribute-set series, deterministic order); leak-0; the reserved-name
  diagnostics entries.
- **M2** Real-host **periodic export thread** (metrics-enabled + feature only; `OTEL_METRIC_EXPORT_INTERVAL`
  default 60s; final flush at teardown). Sandbox unaffected (teardown-only). *Perf gate:* metrics-off a
  hot `counter.add` loop is ~free (gated on `tel_metrics_enabled`); metrics-on within a stated budget
  (aggregation is a map insert + add, off any export path). Report A/B like the tracing gates.
- **M3** Metrics **auto-instrumentation on `server.serve`** *(in-arc — confirmed with user)*: the metrics
  twin of T4's SERVER span. Each accepted request records the OTel-semantic-convention
  `http.server.request.duration` **histogram** (unit `s`, attributes `http.request.method` /
  `http.route` / `http.response.status_code`) and maintains an `http.server.active_requests`
  **UpDownCounter** (+1 on accept, −1 on completion). Rides the same connection hook as the SERVER span
  (`std.http.server` serve loop), gated on `tel_metrics_enabled` so it's free when metrics are off.
  *Gate:* sink-parity oracle — byte-identical `MetricData` (one histogram series per method/route/status,
  active-requests returning to 0) across both backends when a handler runs under the sandbox server; the
  auto-metrics nest with, and reuse the same request boundary as, the existing SERVER-span
  auto-instrumentation. Leak-0.

**Phase D — close-out**
- **D0** Docs: `docs/Observability.md` drops the "tracing, not metrics/logs" caveat and gains logs +
  metrics sections (surface, correlation, the teardown-vs-periodic note, cardinality guidance);
  `Standard-Library-Modules` telemetry entry extended; roadmap ticked. Memory updated
  (`native-otel-arc.md` → signals complete, or a new `native-otel-metrics-logs-arc.md`).

## Optional / deferred (name explicitly, don't silently cut)
- **Async/observable instruments** (`ObservableCounter`/`Gauge` with callbacks) — sync instruments first.
- **Histogram views / custom buckets / delta temporality** — defaults only in this arc.
- ~~**Metrics auto-instrumentation** on `server.serve`~~ — **in-arc as M3** (confirmed with user).
- **stdout/structured-logging bridge** — logs stay OTel-export-only here (§decision 3).
- **Sampling / cardinality limits** — always-on; a hard attribute-set cap is a later policy slice.

## Perf gates (the project bench rule — pinned A/B on a quiet box)
1. **Logs-off / metrics-off ~free** — hot loops calling `info(...)` / `counter.add(...)` with the signal
   disabled must be within noise of the calls removed (cached `tel_logs_enabled`/`tel_metrics_enabled`
   short-circuit before any work).
2. **Signal-on cost bounded** — logs-on: buffer push per record; metrics-on: one map insert + aggregate,
   both off the export path (batched/periodic). State the budget and report both.
