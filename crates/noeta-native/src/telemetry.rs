//! The **Telemetry** capability (native OTEL) — the ABI data types + trait side.
//!
//! Production observability, the 8th [`Host`](crate::Host) capability. A span is a *write-only side
//! effect* — it never re-enters program output — so telemetry is real-host-only and **never
//! differential-tested**: the two backends produce identical `RunResult`s regardless of what they
//! emit. The deterministic sandbox provides an in-memory recorder (in `noeta-stdlib`) purely so
//! conformance can assert on emitted spans; the real host exports OTLP (in `noeta-runtime`).
//!
//! Only **neutral data** crosses this seam — no backend value and no OTel-crate type leaks into the
//! ABI. The `std.telemetry` extension marshals language values into [`AttrValue`]/`&str`, holds the
//! returned [`SpanId`] inside its `Span` extern value, and reads a live span's [`TraceContext`] for
//! propagation; each host accumulates its own [`SpanData`] and consumes it (record vs. export).

use compact_str::CompactString;

/// An opaque handle to one span, minted by [`Telemetry::tel_span_start`] and passed back to the
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

/// **Telemetry** capability (native OTEL) — span emission, the 8th [`Host`](crate::Host)
/// capability. A pure span factory + sink: it creates, mutates, and ends spans and reports a live
/// span's [`TraceContext`], but owns **no active-span stack** — implicit parenting and scope
/// nesting are the `std.telemetry` extension's job (its per-run `ExtState`), so both host impls
/// stay simple and the context model lives in one place.
///
/// The sandbox impl records into an in-memory buffer (deterministic — logical-clock timestamps,
/// derived ids); the real impl exports OTLP. Because a span is never observable in program output,
/// the differential holds by construction with either behind the seam.
pub trait Telemetry {
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
}
