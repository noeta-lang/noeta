//! The **Tracing** capability (native OTEL) — the ABI data types + trait side.
//!
//! Production observability, the 8th [`Host`](crate::Host) capability. A span is a *write-only side
//! effect* — it never re-enters program output — so telemetry is real-host-only and **never
//! differential-tested**: the two backends produce identical `RunResult`s regardless of what they
//! emit. The deterministic sandbox provides an in-memory recorder (in `noeta-stdlib`) purely so
//! conformance can assert on emitted spans; the real host exports OTLP (in `noeta-runtime`).
//!
//! Only **neutral data** crosses this seam — no backend value and no OTel-crate type leaks into the
//! ABI. The `std.tracing` extension marshals language values into [`AttrValue`]/`&str`, holds the
//! returned [`SpanId`] inside its `Span` extern value, and reads a live span's [`TraceContext`] for
//! propagation; each host accumulates its own [`SpanData`] and consumes it (record vs. export).

use compact_str::CompactString;

/// An opaque handle to one span, minted by [`Tracing::tel_span_start`] and passed back to the
/// mutation/end methods. Plain `Send` data (the SDK's `Span` extern value wraps exactly this); ids
/// are per-run and per-host, never reused while a span is live.
pub type SpanId = u64;

/// The OTel span kind — a hint about the span's role in a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

/// A span's terminal status (OTel `Status`). `Error` carries a description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(CompactString),
}

/// An OTel attribute value — the scalar subset of OTel's `AnyValue` the SDK surfaces.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(CompactString),
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// W3C **trace context** — the propagation unit. Serializes to/from a `traceparent` header value
/// (`00-{trace_id:32hex}-{span_id:16hex}-{flags:2hex}`, [W3C Trace Context]). `trace_id`/`span_id`
/// are raw bytes so the wire format is exact; `sampled` is the low bit of the trace-flags.
///
/// [W3C Trace Context]: https://www.w3.org/TR/trace-context/
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub sampled: bool,
}

impl TraceContext {
    /// Format as a W3C `traceparent` value (version `00`).
    pub fn to_traceparent(&self) -> String {
        let mut s = String::with_capacity(55);
        s.push_str("00-");
        for b in &self.trace_id {
            s.push_str(&format!("{b:02x}"));
        }
        s.push('-');
        for b in &self.span_id {
            s.push_str(&format!("{b:02x}"));
        }
        s.push('-');
        s.push_str(if self.sampled { "01" } else { "00" });
        s
    }

    /// Parse a W3C `traceparent` value. Returns `None` on any malformation (per the spec's
    /// forgiving-reader rule, an unparseable inbound header is treated by the caller as "no
    /// parent"). Accepts only version `00`; an all-zero trace-id or span-id is rejected as invalid.
    pub fn parse(s: &str) -> Option<TraceContext> {
        let mut parts = s.trim().split('-');
        let version = parts.next()?;
        let trace_hex = parts.next()?;
        let span_hex = parts.next()?;
        let flags_hex = parts.next()?;
        if parts.next().is_some() || version != "00" {
            return None;
        }
        if trace_hex.len() != 32 || span_hex.len() != 16 || flags_hex.len() != 2 {
            return None;
        }
        let mut trace_id = [0u8; 16];
        let mut span_id = [0u8; 8];
        hex_into(trace_hex, &mut trace_id)?;
        hex_into(span_hex, &mut span_id)?;
        let flags = u8::from_str_radix(flags_hex, 16).ok()?;
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        Some(TraceContext {
            trace_id,
            span_id,
            sampled: flags & 0x01 != 0,
        })
    }
}

/// Decode an even-length hex string into `out` (which fixes the byte count); `None` on a non-hex
/// digit. `out.len()` must be `hex.len() / 2` (the callers size it exactly).
fn hex_into(hex: &str, out: &mut [u8]) -> Option<()> {
    let bytes = hex.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        *slot = (hi * 16 + lo) as u8;
    }
    Some(())
}

/// One event on a span (an OTel `Event`): a name, a wall-time, and attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEvent {
    pub name: CompactString,
    pub unix_ms: u64,
    pub attributes: Vec<(CompactString, AttrValue)>,
}

/// The accumulated record of one span — what a host builds across the `tel_span_*` calls and then
/// records (sandbox) or exports (real host) at end. Neutral data, shared by both host impls.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanData {
    pub name: CompactString,
    pub kind: SpanKind,
    pub context: TraceContext,
    pub parent: Option<TraceContext>,
    pub start_unix_ms: u64,
    pub end_unix_ms: Option<u64>,
    pub attributes: Vec<(CompactString, AttrValue)>,
    pub events: Vec<SpanEvent>,
    pub status: SpanStatus,
}

/// **Tracing** capability (native OTEL) — span emission, the 8th [`Host`](crate::Host)
/// capability. A pure span factory + sink: it creates, mutates, and ends spans and reports a live
/// span's [`TraceContext`], but owns **no active-span stack** — implicit parenting and scope
/// nesting are the `std.tracing` extension's job (its per-run `ExtState`), so both host impls
/// stay simple and the context model lives in one place.
///
/// The sandbox impl records into an in-memory buffer (deterministic — logical-clock timestamps,
/// derived ids); the real impl exports OTLP. Because a span is never observable in program output,
/// the differential holds by construction with either behind the seam.
pub trait Tracing {
    /// Whether telemetry is active — an export sink is configured (real host) or recording is on
    /// (sandbox). **Auto-instrumentation gates on this**: when `false`, `noeta serve` (and other
    /// auto-instrumented boundaries) skip span work entirely, so a program that never configures an
    /// OTLP endpoint pays nothing per request. The explicit `std.tracing` SDK does not consult it —
    /// a user who calls `tracing.span(...)` opted in — so unconfigured spans still mint (and drop at
    /// the null sink), keeping ids/timestamps consistent.
    fn tel_enabled(&self) -> bool;

    /// Start a span. `parent` is `None` for a root span, or the [`TraceContext`] of the parent —
    /// the current active span (read via [`Self::tel_span_context`]) for implicit parenting, or a
    /// remote context parsed from an inbound `traceparent` for propagation. Returns its handle.
    fn tel_span_start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
    ) -> SpanId;

    /// Set (or overwrite) an attribute on a live span.
    fn tel_span_set_attr(&mut self, span: SpanId, key: &str, value: AttrValue);

    /// Append a timestamped event to a live span.
    fn tel_span_add_event(
        &mut self,
        span: SpanId,
        name: &str,
        attrs: Vec<(CompactString, AttrValue)>,
    );

    /// Set a live span's terminal status.
    fn tel_span_set_status(&mut self, span: SpanId, status: SpanStatus);

    /// End a live span (records its wall-time end and hands it to the recorder/exporter). Ending an
    /// unknown or already-ended span is a no-op.
    fn tel_span_end(&mut self, span: SpanId);

    /// The W3C [`TraceContext`] of a live span — for parenting a child or propagating across an
    /// isolate / the wire. An unknown span yields a fresh all-zero-ish context (the SDK only asks
    /// about spans it holds live).
    fn tel_span_context(&mut self, span: SpanId) -> TraceContext;

    /// Intern a **remote** [`TraceContext`] as a local [`SpanId`]-shaped handle (native-otel T5d).
    /// A span handle is per-host, so a context arriving from another isolate (or riding a channel
    /// message) cannot be a live span here — this mints a pseudo-handle whose
    /// [`Self::tel_span_context`] returns exactly the interned context, letting remote parents
    /// live uniformly in the backends' `u64` task-local context stacks (automatic propagation
    /// seeds one at the stack base). The mutation/end methods are no-ops on it (it is not live).
    /// Interning the same context twice may return the same handle.
    fn tel_intern_remote(&mut self, context: TraceContext) -> SpanId;

    /// Whether `span` is a remote-interned handle ([`Self::tel_intern_remote`]) rather than a live
    /// span — the receive-side seeding rule's test: a strand whose context is exactly one remote
    /// seed is "at top level" and may be re-seeded by the next message; a real active span never
    /// is.
    fn tel_is_remote(&self, span: SpanId) -> bool;

    /// Release a remote-interned handle (a seed replaced by the next message's). Keeps a
    /// queue-worker's interned-context table bounded by live strands, not by messages processed.
    /// Releasing a non-remote or already-released id is a no-op.
    fn tel_release_remote(&mut self, span: SpanId);
}

/// An OTel **log severity** — the six named levels the SDK surfaces (OTel defines 24 numeric levels
/// in six groups; this is one per group, the common set). Maps to a severity *number* and *text* at
/// the OTLP encoder (kept out of the ABI so no OTLP detail leaks across the seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// One OTel **log record** — a structured, exported log line. Unlike a `print`, it carries a
/// severity, structured attributes, and (the point of the signal) the [`TraceContext`] of the span
/// active when it was emitted, so a log auto-correlates to the trace that produced it. Neutral
/// `Send` data, shared by both host impls (sandbox records; real host exports OTLP).
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    /// Emission wall-time (unix ms; the host's clock, so deterministic under the sandbox).
    pub unix_ms: u64,
    pub severity: Severity,
    /// The log message (OTel `body`, string form).
    pub body: CompactString,
    pub attributes: Vec<(CompactString, AttrValue)>,
    /// The active span's context at emission, or `None` at top level — the auto-correlation link.
    pub trace_context: Option<TraceContext>,
}

/// **Logging** capability (native OTEL) — the second of three telemetry signals, sibling to
/// [`Tracing`] and [`Metrics`]. Emits OTel [`LogRecord`]s: structured, exported log lines
/// **auto-correlated** to the active span (the record carries the current [`TraceContext`], read
/// from the SDK's task-local context stack), *not* a `print` bridge. Write-only like the other
/// signals — never differential-tested, held by the same byte-identical parity oracle.
pub trait Logging {
    /// Whether the logs signal is active — an OTLP logs endpoint is configured (real host) or the
    /// recorder is on (sandbox). The `std.log` module gates on this so a program that never
    /// configures a logs endpoint pays nothing per `log.info(...)`, mirroring [`Tracing::tel_enabled`].
    fn tel_logs_enabled(&self) -> bool;

    /// Emit a [`LogRecord`] — the sandbox records it into an in-memory buffer (deterministic), the
    /// real host buffers it for OTLP export. The SDK builds the record (severity, body, attributes)
    /// and stamps the active span's [`TraceContext`]; the host owns timestamping only if the SDK
    /// left it unset (it passes `unix_ms` from the host clock, so records stay deterministic).
    fn log_emit(&mut self, record: LogRecord);
}

/// An OTel **instrument kind** — the four synchronous instruments the SDK surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKind {
    /// Monotonic sum (only goes up) — request counts, bytes sent.
    Counter,
    /// Non-monotonic sum (up and down) — active requests, queue depth.
    UpDownCounter,
    /// Distribution over explicit buckets — latencies, sizes.
    Histogram,
    /// Last-value sample — temperature, memory in use.
    Gauge,
}

/// An opaque handle to a host-owned instrument, minted by [`Metrics::metric_get_or_create`] and
/// idempotent on name (the SDK's `Counter`/`Histogram`/`Gauge` extern wraps exactly this, as `Span`
/// wraps a [`SpanId`]).
pub type InstrumentId = u64;

/// A measurement value — instruments accept `int` or `float`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricValue {
    Int(i64),
    Float(f64),
}

impl MetricValue {
    fn as_f64(self) -> f64 {
        match self {
            MetricValue::Int(i) => i as f64,
            MetricValue::Float(f) => f,
        }
    }
}

/// OTel aggregation temporality. This arc emits **cumulative** only (delta is deferred); the enum is
/// carried across the seam so the exporter reads it rather than hard-coding the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality {
    Cumulative,
    Delta,
}

/// A collected number data point (a Sum or Gauge series): its attribute set + value + the cumulative
/// window `[start_unix_ms, unix_ms]`.
#[derive(Debug, Clone, PartialEq)]
pub struct NumberPoint {
    pub attributes: Vec<(CompactString, AttrValue)>,
    pub value: MetricValue,
    pub start_unix_ms: u64,
    pub unix_ms: u64,
}

/// A collected histogram data point: bucket counts over explicit `bounds` (length `bounds.len()+1`,
/// the last bucket is the `+Inf` overflow), plus `count`/`sum`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramPoint {
    pub attributes: Vec<(CompactString, AttrValue)>,
    pub count: u64,
    pub sum: f64,
    pub bounds: Vec<f64>,
    pub buckets: Vec<u64>,
    pub start_unix_ms: u64,
    pub unix_ms: u64,
}

/// The kind-specific collected points of one metric.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricPoints {
    /// Counter / UpDownCounter. `monotonic` distinguishes them (OTel `isMonotonic`).
    Sum {
        points: Vec<NumberPoint>,
        monotonic: bool,
    },
    Gauge(Vec<NumberPoint>),
    Histogram(Vec<HistogramPoint>),
}

/// One collected metric — what [`Metrics::metric_collect`] snapshots and the exporter/recorder
/// consumes. Neutral data, produced identically by both host impls (shared [`MetricStore`]).
#[derive(Debug, Clone, PartialEq)]
pub struct MetricData {
    pub name: CompactString,
    pub unit: CompactString,
    pub temporality: Temporality,
    pub points: MetricPoints,
}

/// The OTel **default explicit histogram bucket boundaries** (the spec's default advice). Config /
/// custom views are deferred; every histogram uses these.
pub const DEFAULT_HISTOGRAM_BOUNDS: &[f64] = &[
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0,
    10000.0,
];

/// **Metrics** capability (native OTEL) — the third telemetry signal, sibling to [`Tracing`] and
/// [`Logging`]. Unlike spans and logs (emit-and-forget), instruments are **long-lived and
/// host-owned**: `metric_get_or_create` is idempotent on name, aggregation (counter sums, histogram
/// buckets) lives entirely host-side keyed by attribute-set (a shared [`MetricStore`] so both
/// backends aggregate byte-identically), and collection snapshots the aggregation for export
/// (sandbox: at teardown only, for determinism; real host: on a periodic reader + a final teardown
/// flush). Write-only like the other signals.
pub trait Metrics {
    /// Whether the metrics signal is active — an OTLP metrics endpoint is configured (real host) or
    /// the recorder is on (sandbox). `std.metrics` and the `server.serve` auto-instrumentation gate
    /// on this so a hot `counter.add(...)` loop is free when metrics are off, mirroring
    /// [`Tracing::tel_enabled`].
    fn tel_metrics_enabled(&self) -> bool;

    /// Get-or-create an instrument by name (idempotent — the OTel "identical instrument" rule; the
    /// first `unit`/`kind` wins). Returns its stable [`InstrumentId`].
    fn metric_get_or_create(
        &mut self,
        name: &str,
        unit: &str,
        kind: InstrumentKind,
    ) -> InstrumentId;

    /// Record a measurement on an instrument, keyed by its attribute set (Counter/UpDownCounter add
    /// to a running sum, Histogram buckets it, Gauge takes the last value — dispatched on the
    /// instrument's kind). `metric_add` and `metric_record` are the same operation; the split is SDK
    /// ergonomics (`add` for counters, `record` for histograms/gauges).
    fn metric_observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        attrs: Vec<(CompactString, AttrValue)>,
    );

    /// Snapshot the current aggregation as collected [`MetricData`] (cumulative), one series per
    /// attribute set, **sorted by attribute-set key** for deterministic ordering. The sandbox calls
    /// this once at teardown; the real host on its periodic reader and a final flush.
    fn metric_collect(&mut self) -> Vec<MetricData>;
}

/// Host-side metric aggregation, shared by both backends so a given call sequence yields
/// byte-identical collected [`MetricData`]. Instruments are stored in creation order (the
/// [`InstrumentId`] is the index); each holds its series keyed by a canonical attribute-set string
/// (a `BTreeMap`, so collection is sorted by that key — the determinism rule).
#[derive(Debug, Clone, Default)]
pub struct MetricStore {
    instruments: Vec<Instrument>,
    by_name: std::collections::HashMap<CompactString, InstrumentId>,
}

#[derive(Debug, Clone)]
struct Instrument {
    name: CompactString,
    unit: CompactString,
    kind: InstrumentKind,
    series: std::collections::BTreeMap<String, Series>,
}

#[derive(Debug, Clone)]
struct Series {
    attributes: Vec<(CompactString, AttrValue)>,
    start_unix_ms: u64,
    agg: Agg,
}

#[derive(Debug, Clone)]
enum Agg {
    Sum(f64, bool), // (running sum, integral) — integral tracks whether every input was an int
    Gauge(MetricValue),
    Histogram {
        count: u64,
        sum: f64,
        buckets: Vec<u64>,
    },
}

impl MetricStore {
    /// Get-or-create an instrument by name (idempotent; first unit/kind wins).
    pub fn get_or_create(&mut self, name: &str, unit: &str, kind: InstrumentKind) -> InstrumentId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = self.instruments.len() as InstrumentId;
        self.instruments.push(Instrument {
            name: name.into(),
            unit: unit.into(),
            kind,
            series: std::collections::BTreeMap::new(),
        });
        self.by_name.insert(name.into(), id);
        id
    }

    /// Record a measurement, dispatched on the instrument's kind. `now` is the host clock (so the
    /// sandbox's logical clock keeps aggregation deterministic).
    pub fn observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        mut attrs: Vec<(CompactString, AttrValue)>,
        now: u64,
    ) {
        let Some(instrument) = self.instruments.get_mut(inst as usize) else {
            return;
        };
        // Sort attributes by key so the canonical key — and the exported attribute order — are
        // independent of the order the caller supplied them.
        attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
        let key = attr_set_key(&attrs);
        let kind = instrument.kind;
        let series = instrument.series.entry(key).or_insert_with(|| Series {
            attributes: attrs,
            start_unix_ms: now,
            agg: Agg::new(kind),
        });
        series.agg.observe(value);
    }

    /// Snapshot every instrument's series as collected [`MetricData`] (cumulative). Instruments in
    /// creation order; series in attribute-key order (the `BTreeMap`). `now` stamps each point.
    pub fn collect(&self, now: u64) -> Vec<MetricData> {
        self.instruments
            .iter()
            .map(|inst| MetricData {
                name: inst.name.clone(),
                unit: inst.unit.clone(),
                temporality: Temporality::Cumulative,
                points: inst.collect_points(now),
            })
            .collect()
    }
}

impl Agg {
    fn new(kind: InstrumentKind) -> Agg {
        match kind {
            InstrumentKind::Counter | InstrumentKind::UpDownCounter => Agg::Sum(0.0, true),
            InstrumentKind::Gauge => Agg::Gauge(MetricValue::Int(0)),
            InstrumentKind::Histogram => Agg::Histogram {
                count: 0,
                sum: 0.0,
                buckets: vec![0; DEFAULT_HISTOGRAM_BOUNDS.len() + 1],
            },
        }
    }

    fn observe(&mut self, value: MetricValue) {
        match self {
            Agg::Sum(sum, integral) => {
                *sum += value.as_f64();
                *integral &= matches!(value, MetricValue::Int(_));
            }
            Agg::Gauge(slot) => *slot = value,
            Agg::Histogram {
                count,
                sum,
                buckets,
            } => {
                let v = value.as_f64();
                *count += 1;
                *sum += v;
                // First bucket whose upper bound `v` does not exceed; else the `+Inf` overflow.
                let idx = DEFAULT_HISTOGRAM_BOUNDS
                    .iter()
                    .position(|&b| v <= b)
                    .unwrap_or(DEFAULT_HISTOGRAM_BOUNDS.len());
                buckets[idx] += 1;
            }
        }
    }
}

impl Instrument {
    fn collect_points(&self, now: u64) -> MetricPoints {
        match self.kind {
            InstrumentKind::Counter | InstrumentKind::UpDownCounter => MetricPoints::Sum {
                monotonic: matches!(self.kind, InstrumentKind::Counter),
                points: self.series.values().map(|s| s.number_point(now)).collect(),
            },
            InstrumentKind::Gauge => {
                MetricPoints::Gauge(self.series.values().map(|s| s.number_point(now)).collect())
            }
            InstrumentKind::Histogram => MetricPoints::Histogram(
                self.series
                    .values()
                    .map(|s| s.histogram_point(now))
                    .collect(),
            ),
        }
    }
}

impl Series {
    fn number_point(&self, now: u64) -> NumberPoint {
        let value = match &self.agg {
            // A Sum of only-int inputs exports as an int (OTel `asInt`); a float input anywhere makes
            // it a double. A Gauge keeps the last value's own type.
            Agg::Sum(sum, true) => MetricValue::Int(*sum as i64),
            Agg::Sum(sum, false) => MetricValue::Float(*sum),
            Agg::Gauge(v) => *v,
            Agg::Histogram { .. } => MetricValue::Int(0), // unreachable for a number series
        };
        NumberPoint {
            attributes: self.attributes.clone(),
            value,
            start_unix_ms: self.start_unix_ms,
            unix_ms: now,
        }
    }

    fn histogram_point(&self, now: u64) -> HistogramPoint {
        let (count, sum, buckets) = match &self.agg {
            Agg::Histogram {
                count,
                sum,
                buckets,
            } => (*count, *sum, buckets.clone()),
            _ => (0, 0.0, vec![0; DEFAULT_HISTOGRAM_BOUNDS.len() + 1]),
        };
        HistogramPoint {
            attributes: self.attributes.clone(),
            count,
            sum,
            bounds: DEFAULT_HISTOGRAM_BOUNDS.to_vec(),
            buckets,
            start_unix_ms: self.start_unix_ms,
            unix_ms: now,
        }
    }
}

/// A canonical, `Ord`-stable string for an attribute set — the `BTreeMap` series key. Attributes are
/// pre-sorted by key; each is rendered `name=<tag>:<value>` so different-typed values never collide
/// (`"1"` string vs `1` int). Floats use `{:?}` for a round-trippable, deterministic rendering.
fn attr_set_key(attrs: &[(CompactString, AttrValue)]) -> String {
    let mut s = String::new();
    for (k, v) in attrs {
        s.push_str(k);
        s.push('=');
        match v {
            AttrValue::Str(x) => {
                s.push_str("s:");
                s.push_str(x);
            }
            AttrValue::Int(x) => {
                s.push_str("i:");
                s.push_str(&x.to_string());
            }
            AttrValue::Float(x) => {
                s.push_str("f:");
                s.push_str(&format!("{x:?}"));
            }
            AttrValue::Bool(x) => {
                s.push_str("b:");
                s.push_str(if *x { "1" } else { "0" });
            }
        }
        s.push(';');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, AttrValue)]) -> Vec<(CompactString, AttrValue)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone()))
            .collect()
    }

    #[test]
    fn store_aggregates_deterministically_across_kinds() {
        let build = || {
            let mut s = MetricStore::default();
            let reqs = s.get_or_create("http.requests", "{request}", InstrumentKind::Counter);
            let active = s.get_or_create("active", "{request}", InstrumentKind::UpDownCounter);
            let dur = s.get_or_create("latency", "ms", InstrumentKind::Histogram);
            let temp = s.get_or_create("temp", "Cel", InstrumentKind::Gauge);

            // Two attribute sets on the counter; note the attrs are supplied in different key order
            // but must land in the same series.
            let get = attrs(&[("method", AttrValue::Str("GET".into()))]);
            let post = attrs(&[("method", AttrValue::Str("POST".into()))]);
            s.observe(reqs, MetricValue::Int(1), get.clone(), 10);
            s.observe(reqs, MetricValue::Int(1), post.clone(), 11);
            s.observe(reqs, MetricValue::Int(1), get.clone(), 12);

            s.observe(active, MetricValue::Int(1), vec![], 10);
            s.observe(active, MetricValue::Int(-1), vec![], 20);

            s.observe(dur, MetricValue::Float(7.0), vec![], 10); // bucket for <=10
            s.observe(dur, MetricValue::Float(3.0), vec![], 11); // bucket for <=5
            s.observe(dur, MetricValue::Int(600), vec![], 12); // bucket for <=750

            s.observe(temp, MetricValue::Float(21.5), vec![], 10);
            s.observe(temp, MetricValue::Float(22.0), vec![], 20); // last-value wins

            // Get-or-create is idempotent on name — a second call with a different unit/kind returns
            // the existing counter (first wins), it does not add an instrument.
            let again = s.get_or_create("http.requests", "ignored", InstrumentKind::Histogram);
            assert_eq!(again, reqs);
            s
        };

        let a = build().collect(100);
        let b = build().collect(100);
        assert_eq!(a, b, "same call sequence → identical collected metrics");

        // Counter: two series (GET=2, POST=1), sorted by attr key so GET precedes POST.
        let MetricPoints::Sum { points, monotonic } = &a[0].points else {
            panic!("counter is a sum");
        };
        assert!(monotonic);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].value, MetricValue::Int(2)); // GET
        assert_eq!(points[1].value, MetricValue::Int(1)); // POST
        assert_eq!(points[0].start_unix_ms, 10);
        assert_eq!(points[0].unix_ms, 100);

        // UpDownCounter: single empty-attr series summing to 0, non-monotonic.
        let MetricPoints::Sum { points, monotonic } = &a[1].points else {
            panic!("updown is a sum");
        };
        assert!(!monotonic);
        assert_eq!(points[0].value, MetricValue::Int(0));

        // Histogram: count 3, sum 610, buckets place 3→(<=5), 7→(<=10), 600→(<=750).
        let MetricPoints::Histogram(points) = &a[2].points else {
            panic!("latency is a histogram");
        };
        assert_eq!(points[0].count, 3);
        assert_eq!(points[0].sum, 610.0);
        assert_eq!(points[0].buckets.len(), DEFAULT_HISTOGRAM_BOUNDS.len() + 1);
        // bounds = [0,5,10,25,50,75,100,250,500,750,…]; a value lands in the first bucket whose
        // upper bound it does not exceed.
        assert_eq!(points[0].buckets[1], 1); // 3.0 → <=5   (bounds[1])
        assert_eq!(points[0].buckets[2], 1); // 7.0 → <=10  (bounds[2])
        assert_eq!(points[0].buckets[9], 1); // 600 → <=750 (bounds[9])
        assert_eq!(points[0].buckets.iter().sum::<u64>(), 3);

        // Gauge: last value wins.
        let MetricPoints::Gauge(points) = &a[3].points else {
            panic!("temp is a gauge");
        };
        assert_eq!(points[0].value, MetricValue::Float(22.0));
    }
}
