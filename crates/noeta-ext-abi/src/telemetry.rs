//! The **Tracing** capability (native OTEL) — the ABI data types + trait side.
//!
//! Production observability, the 8th [`Host`](crate::Host) capability. A span is a *write-only side
//! effect* — it never re-enters program output — so telemetry is real-host-only and **never
//! differential-tested**: the two backends produce identical `RunResult`s regardless of what they
//! emit. The deterministic sandbox provides an in-memory recorder (in `noeta-stdlib`) purely so
//! conformance can assert on emitted spans; the real host exports OTLP (in `noeta-host-real`).
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

/// The OTel **default cardinality limit** — the most distinct attribute sets one instrument
/// aggregates into their own series before folding the rest into the [overflow
/// set](OVERFLOW_ATTRIBUTE_KEY). The spec's own default, so an operator gets the number every other
/// SDK gives them.
pub const DEFAULT_CARDINALITY_LIMIT: usize = 2000;

/// The OTel **overflow attribute** key. The single-attribute set `otel.metric.overflow=true` is the
/// synthetic series every attribute set past the [cardinality limit](DEFAULT_CARDINALITY_LIMIT)
/// aggregates into, so a counter's total stays exact once its breakdown stops.
pub const OVERFLOW_ATTRIBUTE_KEY: &str = "otel.metric.overflow";

/// The overflow attribute set — one boolean attribute, per the spec.
fn overflow_attributes() -> Vec<(CompactString, AttrValue)> {
    vec![(OVERFLOW_ATTRIBUTE_KEY.into(), AttrValue::Bool(true))]
}

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
///
/// Every instrument's series map is capped at [`MetricStore::cardinality_limit`] distinct attribute
/// sets, which is what keeps an instrument carrying a high-cardinality attribute (a request id, a
/// user id) from growing for the life of the process. The cap is **per instrument**, and a set past
/// it folds into the [overflow series](OVERFLOW_ATTRIBUTE_KEY) rather than being dropped.
#[derive(Debug, Clone)]
pub struct MetricStore {
    instruments: Vec<Instrument>,
    by_name: std::collections::HashMap<CompactString, InstrumentId>,
    /// The most distinct attribute sets any one instrument aggregates separately.
    cardinality_limit: usize,
}

impl Default for MetricStore {
    fn default() -> MetricStore {
        MetricStore::with_cardinality_limit(DEFAULT_CARDINALITY_LIMIT)
    }
}

#[derive(Debug, Clone)]
struct Instrument {
    name: CompactString,
    unit: CompactString,
    kind: InstrumentKind,
    /// Series for ordinary attribute sets. `len()` is therefore the exact count the cardinality
    /// limit is checked against — the overflow series is deliberately *not* in here.
    series: std::collections::BTreeMap<String, Series>,
    /// The single `otel.metric.overflow=true` series, minted the first time a new attribute set
    /// arrives past the limit and collected after the ordinary ones.
    overflow: Option<Series>,
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
    /// A store whose instruments each aggregate at most `limit` distinct attribute sets separately.
    /// `0` folds every attributed measurement into the overflow series immediately; callers that
    /// treat zero as "unconfigured" resolve that before they get here.
    pub fn with_cardinality_limit(limit: usize) -> MetricStore {
        MetricStore {
            instruments: Vec::new(),
            by_name: std::collections::HashMap::new(),
            cardinality_limit: limit,
        }
    }

    /// The per-instrument cardinality limit this store enforces.
    pub fn cardinality_limit(&self) -> usize {
        self.cardinality_limit
    }

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
            overflow: None,
        });
        self.by_name.insert(name.into(), id);
        id
    }

    /// Record a measurement, dispatched on the instrument's kind. `now` is the host clock (so the
    /// sandbox's logical clock keeps aggregation deterministic).
    ///
    /// An attribute set the instrument already tracks always reaches its own series. A **new** set
    /// arriving once the instrument holds [`Self::cardinality_limit`] of them folds into the single
    /// `otel.metric.overflow=true` series instead, so the instrument's series map is bounded while
    /// every measurement still lands in exactly one series — a counter's total stays exact, only its
    /// breakdown stops.
    ///
    /// The overflow series aggregates by the instrument's own kind, so what "folding" costs differs
    /// by kind: a sum keeps its total and a histogram keeps its count, sum and distribution, but a
    /// **gauge** is last-value, so folded measurements overwrite each other and only the most recent
    /// survives. That loss belongs to gauge aggregation rather than to this cap — there is no total
    /// for a gauge to preserve — and it is deterministic (observation order), so both backends still
    /// collect the same value.
    pub fn observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        mut attrs: Vec<(CompactString, AttrValue)>,
        now: u64,
    ) {
        let limit = self.cardinality_limit;
        let Some(instrument) = self.instruments.get_mut(inst as usize) else {
            return;
        };
        // Sort attributes by key so the canonical key — and the exported attribute order — are
        // independent of the order the caller supplied them.
        attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
        let key = attr_set_key(&attrs);
        let kind = instrument.kind;
        // Under the limit — the case every healthy instrument stays in — this costs one integer
        // compare and nothing else; the extra `contains_key` is only paid once the instrument is
        // already at its cap, which is a regime it should not be in.
        let overflow = is_overflow_set(&attrs)
            || (instrument.series.len() >= limit && !instrument.series.contains_key(&key));
        let series = if overflow {
            instrument.overflow.get_or_insert_with(|| Series {
                attributes: overflow_attributes(),
                start_unix_ms: now,
                agg: Agg::new(kind),
            })
        } else {
            instrument.series.entry(key).or_insert_with(|| Series {
                attributes: attrs,
                start_unix_ms: now,
                agg: Agg::new(kind),
            })
        };
        series.agg.observe(value);
    }

    /// Snapshot every instrument's series as collected [`MetricData`] (cumulative). Instruments in
    /// creation order; ordinary series in attribute-key order (the `BTreeMap`), the overflow series
    /// last. `now` stamps each point.
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
    /// Every series to collect: the ordinary ones in attribute-key order, then the overflow series
    /// if this instrument ever exceeded the cardinality limit.
    fn all_series(&self) -> impl Iterator<Item = &Series> {
        self.series.values().chain(self.overflow.iter())
    }

    fn collect_points(&self, now: u64) -> MetricPoints {
        match self.kind {
            InstrumentKind::Counter | InstrumentKind::UpDownCounter => MetricPoints::Sum {
                monotonic: matches!(self.kind, InstrumentKind::Counter),
                points: self.all_series().map(|s| s.number_point(now)).collect(),
            },
            InstrumentKind::Gauge => {
                MetricPoints::Gauge(self.all_series().map(|s| s.number_point(now)).collect())
            }
            InstrumentKind::Histogram => {
                MetricPoints::Histogram(self.all_series().map(|s| s.histogram_point(now)).collect())
            }
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

/// Whether `attrs` *is* the overflow set. A program free to choose its own attribute keys can name
/// `otel.metric.overflow` itself, and that measurement belongs in the one overflow series rather
/// than in a second, indistinguishable data point beside it.
fn is_overflow_set(attrs: &[(CompactString, AttrValue)]) -> bool {
    matches!(attrs, [(key, AttrValue::Bool(true))] if key == OVERFLOW_ATTRIBUTE_KEY)
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

/// Live-span bookkeeping for hosts with **no exporter** (the WASI and browser hosts — P-WASM):
/// spans mint real W3C ids from caller-supplied entropy, live contexts serve parenting and
/// propagation, remote contexts intern as pseudo-handles, and an ended span is handed back for
/// the caller to drop (the null sink) — so `tel_span_context` on it yields the fresh zero
/// context, exactly the `RealHost`-without-`telemetry` semantics. `RealHost` and the sandbox
/// keep their own state (an exporter buffer and a deterministic recorder respectively); this
/// struct is the shared shape for hosts that track without emitting.
#[derive(Debug)]
pub struct SpanTracker {
    /// Opaque span-handle counter (shared with remote interns, so the id spaces cannot collide).
    next_span: u64,
    /// In-flight spans by handle, ended entries removed.
    live: std::collections::HashMap<SpanId, SpanData>,
    /// Remote-interned contexts: read by [`SpanTracker::context`], no-ops elsewhere.
    remote: std::collections::HashMap<SpanId, TraceContext>,
}

impl Default for SpanTracker {
    fn default() -> SpanTracker {
        SpanTracker {
            next_span: 1,
            live: std::collections::HashMap::new(),
            remote: std::collections::HashMap::new(),
        }
    }
}

impl SpanTracker {
    /// Start a span: `span_id`/`fresh_trace_id` are caller-drawn entropy (a child inherits its
    /// parent's trace id instead of `fresh_trace_id`), `now` the host's wall clock.
    pub fn start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
        span_id: [u8; 8],
        fresh_trace_id: [u8; 16],
        now: u64,
    ) -> SpanId {
        let handle = self.next_span;
        self.next_span += 1;
        let context = TraceContext {
            trace_id: parent.map_or(fresh_trace_id, |p| p.trace_id),
            span_id,
            sampled: true,
        };
        self.live.insert(
            handle,
            SpanData {
                name: name.into(),
                kind,
                context,
                parent,
                start_unix_ms: now,
                end_unix_ms: None,
                attributes: Vec::new(),
                events: Vec::new(),
                status: SpanStatus::Unset,
            },
        );
        handle
    }

    /// Set (or overwrite) an attribute on a live span.
    pub fn set_attr(&mut self, span: SpanId, key: &str, value: AttrValue) {
        if let Some(s) = self.live.get_mut(&span) {
            match s.attributes.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value,
                None => s.attributes.push((key.into(), value)),
            }
        }
    }

    /// Append a timestamped event to a live span.
    pub fn add_event(
        &mut self,
        span: SpanId,
        name: &str,
        attrs: Vec<(CompactString, AttrValue)>,
        now: u64,
    ) {
        if let Some(s) = self.live.get_mut(&span) {
            s.events.push(SpanEvent {
                name: name.into(),
                unix_ms: now,
                attributes: attrs,
            });
        }
    }

    /// Set a live span's terminal status.
    pub fn set_status(&mut self, span: SpanId, status: SpanStatus) {
        if let Some(s) = self.live.get_mut(&span) {
            s.status = status;
        }
    }

    /// End a live span, returning its completed record — the caller drops it (null sink) or
    /// exports it. `None` for an unknown/already-ended span (a no-op end).
    pub fn end(&mut self, span: SpanId, now: u64) -> Option<SpanData> {
        let mut data = self.live.remove(&span)?;
        data.end_unix_ms = Some(now);
        Some(data)
    }

    /// The context of a live span or remote intern; the fresh zero context for anything else.
    pub fn context(&self, span: SpanId) -> TraceContext {
        if let Some(remote) = self.remote.get(&span) {
            return *remote;
        }
        self.live.get(&span).map_or(
            TraceContext {
                trace_id: [0u8; 16],
                span_id: [0u8; 8],
                sampled: false,
            },
            |s| s.context,
        )
    }

    /// Intern a remote context as a pseudo-handle (shares the span counter — no collisions).
    pub fn intern_remote(&mut self, context: TraceContext) -> SpanId {
        let id = self.next_span;
        self.next_span += 1;
        self.remote.insert(id, context);
        id
    }

    /// Whether `span` is a remote-interned handle.
    pub fn is_remote(&self, span: SpanId) -> bool {
        self.remote.contains_key(&span)
    }

    /// Release a remote-interned handle.
    pub fn release_remote(&mut self, span: SpanId) {
        self.remote.remove(&span);
    }
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

    /// One attribute set per distinct value of `key`, `count` of them, each recording `1`.
    fn observe_distinct(store: &mut MetricStore, inst: InstrumentId, key: &str, count: usize) {
        for i in 0..count {
            store.observe(
                inst,
                MetricValue::Int(1),
                attrs(&[(key, AttrValue::Int(i as i64))]),
                10,
            );
        }
    }

    fn sum_points(metric: &MetricData) -> &[NumberPoint] {
        match &metric.points {
            MetricPoints::Sum { points, .. } => points,
            _ => panic!("expected a sum"),
        }
    }

    fn is_overflow_point(point: &NumberPoint) -> bool {
        point.attributes.as_slice()
            == [(OVERFLOW_ATTRIBUTE_KEY.into(), AttrValue::Bool(true))].as_slice()
    }

    /// **The growth demonstration, bounded.** A counter carrying a distinct attribute value per
    /// measurement — a request id, the mistake the limit exists for — keeps a bounded number of
    /// series no matter how many measurements arrive, and the number is the configured limit plus
    /// the one overflow series. Without the cap this collects 5,000 points and would keep climbing
    /// for the life of the process.
    #[test]
    fn a_distinct_attribute_per_measurement_stays_bounded() {
        let mut store = MetricStore::with_cardinality_limit(64);
        let reqs = store.get_or_create("http.requests", "{request}", InstrumentKind::Counter);
        observe_distinct(&mut store, reqs, "request.id", 5_000);

        let collected = store.collect(100);
        let points = sum_points(&collected[0]);
        assert_eq!(
            points.len(),
            65,
            "64 ordinary series plus the one overflow series, whatever the input cardinality"
        );
        assert_eq!(
            points.iter().filter(|p| is_overflow_point(p)).count(),
            1,
            "exactly one overflow series"
        );
    }

    /// The limit is **per instrument**, not a budget shared across the store: two counters each
    /// reach their own cap, and neither one's cardinality pushes the other into overflow.
    #[test]
    fn the_limit_is_per_instrument() {
        let mut store = MetricStore::with_cardinality_limit(4);
        let a = store.get_or_create("a", "", InstrumentKind::Counter);
        let b = store.get_or_create("b", "", InstrumentKind::Counter);
        observe_distinct(&mut store, a, "id", 100);
        observe_distinct(&mut store, b, "id", 4);

        let collected = store.collect(100);
        assert_eq!(sum_points(&collected[0]).len(), 5, "4 series + overflow");
        assert_eq!(
            sum_points(&collected[1]).len(),
            4,
            "`b` used exactly its budget, so nothing overflowed on it"
        );
        assert!(
            !sum_points(&collected[1]).iter().any(is_overflow_point),
            "the spec's guarantee: no overflow while the distinct sets are within the limit"
        );
    }

    /// The cap reached **exactly** — the spec's guarantee that overflow does not happen while the
    /// number of distinct non-overflow attribute sets is less than or equal to the limit. The next
    /// distinct set is the first one to fold.
    #[test]
    fn the_limit_is_reached_exactly_before_anything_overflows() {
        for (distinct, expected_points, overflowed) in [(3, 3, false), (4, 4, false), (5, 5, true)]
        {
            let mut store = MetricStore::with_cardinality_limit(4);
            let inst = store.get_or_create("c", "", InstrumentKind::Counter);
            observe_distinct(&mut store, inst, "id", distinct);

            let collected = store.collect(100);
            let points = sum_points(&collected[0]);
            assert_eq!(
                points.len(),
                expected_points,
                "{distinct} distinct sets under a limit of 4"
            );
            assert_eq!(
                points.iter().any(is_overflow_point),
                overflowed,
                "{distinct} distinct sets under a limit of 4 — overflow expected: {overflowed}"
            );
        }
    }

    /// Folding, not dropping: the counter's **total** is exactly the number of measurements, and
    /// every measurement is reflected in exactly one series. A dropping policy would lose the
    /// difference silently, which is the whole reason the spec folds.
    #[test]
    fn overflow_folding_preserves_the_counters_total() {
        let mut store = MetricStore::with_cardinality_limit(10);
        let inst = store.get_or_create("c", "", InstrumentKind::Counter);
        observe_distinct(&mut store, inst, "id", 1_000);

        let collected = store.collect(100);
        let points = sum_points(&collected[0]);
        let total: i64 = points
            .iter()
            .map(|p| match p.value {
                MetricValue::Int(i) => i,
                MetricValue::Float(f) => f as i64,
            })
            .sum();
        assert_eq!(total, 1_000, "no measurement is dropped or double-counted");
        let overflow = points
            .iter()
            .find(|p| is_overflow_point(p))
            .expect("folded");
        assert_eq!(
            overflow.value,
            MetricValue::Int(990),
            "the 10 sets seen first keep their own series; the other 990 fold into one"
        );
    }

    /// The overflow series is exactly the spec's attribute set — one boolean attribute
    /// `otel.metric.overflow=true` — and it collects **after** the ordinary series, which stay in
    /// attribute-key order.
    #[test]
    fn the_overflow_series_is_the_specs_attribute_set_and_collects_last() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("c", "", InstrumentKind::Counter);
        for value in ["a", "b", "c", "d"] {
            store.observe(
                inst,
                MetricValue::Int(1),
                attrs(&[("route", AttrValue::Str(value.into()))]),
                10,
            );
        }

        let collected = store.collect(100);
        let points = sum_points(&collected[0]);
        assert_eq!(points.len(), 3);
        assert_eq!(
            points[0].attributes,
            attrs(&[("route", AttrValue::Str("a".into()))])
        );
        assert_eq!(
            points[1].attributes,
            attrs(&[("route", AttrValue::Str("b".into()))])
        );
        assert_eq!(
            points[2].attributes,
            vec![(OVERFLOW_ATTRIBUTE_KEY.into(), AttrValue::Bool(true))],
            "the synthetic set the spec names, last"
        );
    }

    /// Cumulative temporality: an attribute set observed **before** overflow began keeps its own
    /// series and keeps accumulating afterwards. Only sets first seen after the cap fold.
    #[test]
    fn sets_seen_before_overflow_keep_their_own_series() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("c", "", InstrumentKind::Counter);
        let get = attrs(&[("method", AttrValue::Str("GET".into()))]);
        let post = attrs(&[("method", AttrValue::Str("POST".into()))]);
        store.observe(inst, MetricValue::Int(1), get.clone(), 10);
        store.observe(inst, MetricValue::Int(1), post.clone(), 10);
        observe_distinct(&mut store, inst, "id", 50); // all of these overflow
        store.observe(inst, MetricValue::Int(1), get.clone(), 20); // still its own series

        let collected = store.collect(100);
        let points = sum_points(&collected[0]);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].attributes, get);
        assert_eq!(
            points[0].value,
            MetricValue::Int(2),
            "GET kept accumulating"
        );
        assert_eq!(points[1].attributes, post);
        assert_eq!(points[1].value, MetricValue::Int(1));
        assert!(is_overflow_point(&points[2]));
        assert_eq!(points[2].value, MetricValue::Int(50));
    }

    /// **A histogram's `count` and `sum` survive folding, and so does its bucket distribution.** The
    /// overflow series is an ordinary histogram aggregator over the folded measurements, not a fresh
    /// start with different bounds: it carries the same [`DEFAULT_HISTOGRAM_BOUNDS`], its buckets
    /// account for every folded observation, and the instrument's total count and total sum across
    /// all series are still exactly what was recorded. The counter test above says the same thing
    /// about a sum; a histogram has three numbers to lose instead of one.
    #[test]
    fn overflow_folding_preserves_a_histograms_count_sum_and_buckets() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("h", "ms", InstrumentKind::Histogram);
        // Six distinct attribute sets, one measurement each. The first two keep their own series;
        // the last four fold into one.
        let values = [3.0f64, 7.0, 600.0, 3.0, 7.0, 600.0];
        for (i, v) in values.iter().enumerate() {
            store.observe(
                inst,
                MetricValue::Float(*v),
                attrs(&[("id", AttrValue::Int(i as i64))]),
                10,
            );
        }

        let collected = store.collect(100);
        let MetricPoints::Histogram(points) = &collected[0].points else {
            panic!("histogram");
        };
        assert_eq!(points.len(), 3, "2 ordinary series + the overflow series");

        let overflow = points.last().expect("a series");
        assert_eq!(
            overflow.attributes,
            vec![(OVERFLOW_ATTRIBUTE_KEY.into(), AttrValue::Bool(true))],
            "the last point is the overflow series"
        );
        assert_eq!(overflow.count, 4, "every folded measurement is counted");
        assert_eq!(
            overflow.sum,
            600.0 + 3.0 + 7.0 + 600.0,
            "and every folded measurement is summed"
        );
        assert_eq!(
            overflow.bounds, DEFAULT_HISTOGRAM_BOUNDS,
            "the overflow series buckets on the same bounds as every other series"
        );
        // bounds = [0,5,10,25,50,75,100,250,500,750,…]; the folded 3 → <=5, 7 → <=10, 600 ×2 → <=750.
        assert_eq!(overflow.buckets[1], 1);
        assert_eq!(overflow.buckets[2], 1);
        assert_eq!(overflow.buckets[9], 2);
        assert_eq!(
            overflow.buckets.iter().sum::<u64>(),
            overflow.count,
            "the distribution accounts for exactly the folded measurements"
        );

        // The instrument as a whole still reports what it was given — nothing dropped, nothing
        // double-counted, which is the same guarantee the counter's total gets.
        assert_eq!(
            points.iter().map(|p| p.count).sum::<u64>(),
            values.len() as u64
        );
        assert_eq!(
            points.iter().map(|p| p.sum).sum::<f64>(),
            values.iter().sum::<f64>()
        );
    }

    /// **A gauge has no total to preserve, and folding says so plainly.** Its aggregation is
    /// last-value, so the overflow series holds the most recent folded measurement and the earlier
    /// folded ones are gone — that loss is inherent to gauge aggregation, not to the overflow
    /// policy, and the spec asks only that every measurement reach exactly one aggregator. The
    /// survivor is the *last* one in observation order, so it is deterministic rather than
    /// arbitrary, which is what lets both backends collect identical gauges.
    #[test]
    fn a_folded_gauge_keeps_the_last_measurement() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("g", "", InstrumentKind::Gauge);
        for (i, v) in [10i64, 20, 30, 40, 50].iter().enumerate() {
            store.observe(
                inst,
                MetricValue::Int(*v),
                attrs(&[("id", AttrValue::Int(i as i64))]),
                10,
            );
        }

        let collected = store.collect(100);
        let MetricPoints::Gauge(points) = &collected[0].points else {
            panic!("gauge");
        };
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].value, MetricValue::Int(10), "its own series");
        assert_eq!(points[1].value, MetricValue::Int(20), "its own series");
        assert!(is_overflow_point(points.last().expect("a series")));
        assert_eq!(
            points[2].value,
            MetricValue::Int(50),
            "the last folded measurement wins — a gauge overwrites, and 30 and 40 are gone"
        );
    }

    /// **The overflow series' cumulative start is the moment folding began, and it never moves.** A
    /// cumulative data point whose `start_unix_ms` walked forward with each fold would tell a
    /// collector the series had restarted, which is how a cumulative exporter loses a rate.
    #[test]
    fn the_overflow_series_start_time_is_when_folding_began_and_does_not_move() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("c", "", InstrumentKind::Counter);
        // Each measurement carries a later clock reading than the one before it.
        for (i, now) in [100u64, 200, 300, 400, 500].iter().enumerate() {
            store.observe(
                inst,
                MetricValue::Int(1),
                attrs(&[("id", AttrValue::Int(i as i64))]),
                *now,
            );
        }

        let collected = store.collect(999);
        let points = sum_points(&collected[0]);
        assert_eq!(points[0].start_unix_ms, 100, "when that series started");
        assert_eq!(points[1].start_unix_ms, 200, "when that series started");

        let overflow = points.last().expect("a series");
        assert!(is_overflow_point(overflow));
        assert_eq!(
            overflow.start_unix_ms, 300,
            "the third distinct set was the first to fold, so folding began at 300 — not 400 or \
             500, which would mean the series restarted twice"
        );
        assert_eq!(overflow.unix_ms, 999, "collection stamps the window's end");
        assert_eq!(overflow.value, MetricValue::Int(3), "three sets folded");
    }

    /// Every instrument kind is capped, not just counters — a histogram's buckets and a gauge's
    /// last value are just as unbounded per attribute set.
    #[test]
    fn histograms_and_gauges_are_capped_too() {
        let mut store = MetricStore::with_cardinality_limit(3);
        let hist = store.get_or_create("h", "ms", InstrumentKind::Histogram);
        let gauge = store.get_or_create("g", "", InstrumentKind::Gauge);
        observe_distinct(&mut store, hist, "id", 100);
        observe_distinct(&mut store, gauge, "id", 100);

        let collected = store.collect(100);
        let MetricPoints::Histogram(points) = &collected[0].points else {
            panic!("histogram");
        };
        assert_eq!(points.len(), 4, "3 series + overflow");
        assert_eq!(
            points[3].count, 97,
            "the folded measurements are all counted in the overflow bucket"
        );
        let MetricPoints::Gauge(points) = &collected[1].points else {
            panic!("gauge");
        };
        assert_eq!(points.len(), 4);
    }

    /// A program may name `otel.metric.overflow` itself. That measurement belongs in the one
    /// overflow series — two data points carrying identical attributes would be indistinguishable
    /// on the wire — and it does not consume a slot of the instrument's ordinary budget.
    #[test]
    fn a_program_supplied_overflow_set_folds_into_the_one_overflow_series() {
        let mut store = MetricStore::with_cardinality_limit(2);
        let inst = store.get_or_create("c", "", InstrumentKind::Counter);
        store.observe(
            inst,
            MetricValue::Int(7),
            attrs(&[(OVERFLOW_ATTRIBUTE_KEY, AttrValue::Bool(true))]),
            10,
        );
        // Two ordinary sets still fit: the program's overflow measurement took none of the budget.
        observe_distinct(&mut store, inst, "id", 2);

        let collected = store.collect(100);
        let points = sum_points(&collected[0]);
        assert_eq!(
            points.len(),
            3,
            "2 ordinary series + the single overflow one"
        );
        assert_eq!(
            points.iter().filter(|p| is_overflow_point(p)).count(),
            1,
            "never two points with the same attributes"
        );
        assert_eq!(points[2].value, MetricValue::Int(7));
    }

    /// The default store carries the spec's default limit, so every host that builds one — the
    /// sandbox, the WASI host, the browser host — is capped without configuring anything.
    #[test]
    fn the_default_store_carries_the_spec_default_limit() {
        assert_eq!(
            MetricStore::default().cardinality_limit(),
            DEFAULT_CARDINALITY_LIMIT
        );
        assert_eq!(DEFAULT_CARDINALITY_LIMIT, 2000, "the OTel spec's default");
    }
}
