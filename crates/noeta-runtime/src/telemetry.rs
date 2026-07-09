//! RealHost's OTLP/HTTP-JSON span exporter (native OTEL) — behind the `telemetry` feature.
//!
//! Deliberately **hand-rolled over the `reqwest` + `serde_json` already compiled**, so a telemetry
//! build adds no protobuf/gRPC tree (no `opentelemetry-otlp`, no `tonic`/`prost`) — the bundle
//! decision recorded in `plans/native-otel/README.md`. Configuration comes from the standard
//! `OTEL_EXPORTER_OTLP_*` / `OTEL_SERVICE_NAME` env vars; with no endpoint set the exporter is
//! absent and spans are dropped (the null sink), so an un-configured program pays ~nothing.
//!
//! T0 is a skeleton: spans buffer and flush as one OTLP/JSON POST at a size threshold or on host
//! teardown. A real batch span processor (time-triggered, off the request path) is a later slice.

use noeta_stdlib::{AttrValue, LogRecord, Severity, SpanData, SpanKind, SpanStatus};
use serde_json::{Value, json};

/// Flush the span buffer once it reaches this many spans (a minimal batch; the proper time-based
/// batch processor is a later slice).
pub(crate) const FLUSH_THRESHOLD: usize = 512;

/// A configured OTLP/HTTP-JSON exporter, shared by all three telemetry signals. Present when **any**
/// signal has a resolved endpoint; each signal's endpoint is `Some` only when that signal is enabled
/// (a base endpoint is set and the signal isn't turned off via `OTEL_{SIGNAL}_EXPORTER=none`, or a
/// signal-specific endpoint is set). Headers and `service.name` are shared across signals.
pub(crate) struct OtlpExporter {
    /// The full traces endpoint URL (`.../v1/traces`), or `None` when traces are disabled.
    pub(crate) traces_endpoint: Option<String>,
    /// The full logs endpoint URL (`.../v1/logs`), or `None` when logs are disabled.
    pub(crate) logs_endpoint: Option<String>,
    /// The full metrics endpoint URL (`.../v1/metrics`), or `None` when metrics are disabled.
    pub(crate) metrics_endpoint: Option<String>,
    /// Extra headers (e.g. an auth token), from `OTEL_EXPORTER_OTLP_HEADERS`. Shared across signals.
    pub(crate) headers: Vec<(String, String)>,
    /// `service.name` resource attribute. Shared across signals.
    service_name: String,
}

// Redacting Debug — header values may carry secrets (auth tokens), so they never print.
impl std::fmt::Debug for OtlpExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpExporter")
            .field("traces_endpoint", &self.traces_endpoint)
            .field("logs_endpoint", &self.logs_endpoint)
            .field("metrics_endpoint", &self.metrics_endpoint)
            .field(
                "headers",
                &format_args!("<{} redacted>", self.headers.len()),
            )
            .field("service_name", &self.service_name)
            .finish()
    }
}

impl OtlpExporter {
    /// Build from the environment, or `None` if no OTLP endpoint is configured for any signal (the
    /// null sink). The master switch is `OTEL_EXPORTER_OTLP_ENDPOINT` (base, with `/v1/{signal}`
    /// appended per signal); a signal-specific `OTEL_EXPORTER_OTLP_{SIGNAL}_ENDPOINT` wins for that
    /// signal (used verbatim) and can enable one signal on its own. Any signal can be turned off with
    /// `OTEL_{SIGNAL}_EXPORTER=none` even when a base is set.
    pub(crate) fn from_env() -> Option<OtlpExporter> {
        // The standard global kill switch (OTel spec): `OTEL_SDK_DISABLED=true` forces the null sink
        // even when an endpoint is configured — the opt-out for auto-instrumentation.
        if std::env::var("OTEL_SDK_DISABLED").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
            return None;
        }
        let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty());
        let traces_endpoint = signal_endpoint(base.as_deref(), "traces");
        let logs_endpoint = signal_endpoint(base.as_deref(), "logs");
        let metrics_endpoint = signal_endpoint(base.as_deref(), "metrics");
        // No signal resolved to an endpoint ⇒ the null sink, exactly as an unset base was before.
        if traces_endpoint.is_none() && logs_endpoint.is_none() && metrics_endpoint.is_none() {
            return None;
        }
        let headers = std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
            .ok()
            .map(|s| parse_headers(&s))
            .unwrap_or_default();
        let service_name =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "noeta".to_string());
        Some(OtlpExporter {
            traces_endpoint,
            logs_endpoint,
            metrics_endpoint,
            headers,
            service_name,
        })
    }

    /// The OTLP/JSON `ExportTraceServiceRequest` body for `spans`.
    pub(crate) fn request_body(&self, spans: &[SpanData]) -> Value {
        spans_to_json(spans, &self.service_name)
    }

    /// The OTLP/JSON `ExportLogsServiceRequest` body for `records`.
    pub(crate) fn logs_request_body(&self, records: &[LogRecord]) -> Value {
        logs_to_json(records, &self.service_name)
    }
}

/// Resolve one signal's endpoint from the environment. A signal-specific
/// `OTEL_EXPORTER_OTLP_{SIGNAL}_ENDPOINT` is used verbatim; otherwise `base` gets `/v1/{signal}`
/// appended. `OTEL_{SIGNAL}_EXPORTER=none` disables the signal outright. `signal` is the lowercase
/// path segment (`traces`/`logs`/`metrics`); the env var infix is its uppercase form.
fn signal_endpoint(base: Option<&str>, signal: &str) -> Option<String> {
    let upper = signal.to_ascii_uppercase();
    if std::env::var(format!("OTEL_{upper}_EXPORTER")).is_ok_and(|v| v.eq_ignore_ascii_case("none")) {
        return None;
    }
    if let Some(ep) = std::env::var(format!("OTEL_EXPORTER_OTLP_{upper}_ENDPOINT"))
        .ok()
        .filter(|s| !s.is_empty())
    {
        return Some(ep);
    }
    base.map(|b| format!("{}/v1/{signal}", b.trim_end_matches('/')))
}

/// The OTLP `resource` object carrying `service.name` — shared by all three signals' request bodies.
pub(crate) fn resource(service_name: &str) -> Value {
    json!({ "attributes": [attr_kv("service.name", &AttrValue::Str(service_name.into()))] })
}

/// Parse `OTEL_EXPORTER_OTLP_HEADERS` (`k1=v1,k2=v2`) into pairs, skipping malformed entries.
fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Build the OTLP/JSON `ExportTraceServiceRequest` for a batch of spans (one resource, one scope).
/// Per the OTLP/JSON encoding: `traceId`/`spanId` are **hex** strings (the documented exception to
/// the base64-for-bytes rule), int64/uint64 fields (`intValue`, `*TimeUnixNano`) are **strings**,
/// `kind`/`status.code` are enum ints.
pub(crate) fn spans_to_json(spans: &[SpanData], service_name: &str) -> Value {
    let spans_json: Vec<Value> = spans.iter().map(span_to_json).collect();
    json!({
        "resourceSpans": [{
            "resource": resource(service_name),
            "scopeSpans": [{
                "scope": { "name": "noeta", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans_json
            }]
        }]
    })
}

fn span_to_json(s: &SpanData) -> Value {
    let mut obj = json!({
        "traceId": hex(&s.context.trace_id),
        "spanId": hex(&s.context.span_id),
        "name": s.name.as_str(),
        "kind": kind_code(s.kind),
        "startTimeUnixNano": nanos(s.start_unix_ms),
        "endTimeUnixNano": nanos(s.end_unix_ms.unwrap_or(s.start_unix_ms)),
        "attributes": s.attributes.iter().map(|(k, v)| attr_kv(k, v)).collect::<Vec<_>>(),
        "events": s.events.iter().map(event_to_json).collect::<Vec<_>>(),
        "status": status_to_json(&s.status),
    });
    if let Some(parent) = s.parent {
        obj["parentSpanId"] = Value::String(hex(&parent.span_id));
    }
    obj
}

/// Build the OTLP/JSON `ExportLogsServiceRequest` for a batch of log records (one resource, one
/// scope). Same encoding rules as spans: `timeUnixNano` is a stringified uint64, `traceId`/`spanId`
/// are hex, `severityNumber` is the OTel numeric level, `body` is an `AnyValue` (`stringValue`).
pub(crate) fn logs_to_json(records: &[LogRecord], service_name: &str) -> Value {
    let records_json: Vec<Value> = records.iter().map(log_to_json).collect();
    json!({
        "resourceLogs": [{
            "resource": resource(service_name),
            "scopeLogs": [{
                "scope": { "name": "noeta", "version": env!("CARGO_PKG_VERSION") },
                "logRecords": records_json
            }]
        }]
    })
}

fn log_to_json(r: &LogRecord) -> Value {
    let mut obj = json!({
        "timeUnixNano": nanos(r.unix_ms),
        "observedTimeUnixNano": nanos(r.unix_ms),
        "severityNumber": severity_number(r.severity),
        "severityText": severity_text(r.severity),
        "body": { "stringValue": r.body.as_str() },
        "attributes": r.attributes.iter().map(|(k, v)| attr_kv(k, v)).collect::<Vec<_>>(),
    });
    // Auto-correlation: a record emitted inside a span carries that span's trace/span ids.
    if let Some(ctx) = &r.trace_context {
        obj["traceId"] = Value::String(hex(&ctx.trace_id));
        obj["spanId"] = Value::String(hex(&ctx.span_id));
    }
    obj
}

/// The OTel severity **number** for a level (one representative per severity group: TRACE=1,
/// DEBUG=5, INFO=9, WARN=13, ERROR=17, FATAL=21 — the group base values).
fn severity_number(s: Severity) -> u8 {
    match s {
        Severity::Trace => 1,
        Severity::Debug => 5,
        Severity::Info => 9,
        Severity::Warn => 13,
        Severity::Error => 17,
        Severity::Fatal => 21,
    }
}

/// The OTel severity **text** for a level (the conventional uppercase name).
fn severity_text(s: Severity) -> &'static str {
    match s {
        Severity::Trace => "TRACE",
        Severity::Debug => "DEBUG",
        Severity::Info => "INFO",
        Severity::Warn => "WARN",
        Severity::Error => "ERROR",
        Severity::Fatal => "FATAL",
    }
}

fn event_to_json(e: &noeta_stdlib::SpanEvent) -> Value {
    json!({
        "timeUnixNano": nanos(e.unix_ms),
        "name": e.name.as_str(),
        "attributes": e.attributes.iter().map(|(k, v)| attr_kv(k, v)).collect::<Vec<_>>(),
    })
}

fn status_to_json(status: &SpanStatus) -> Value {
    match status {
        SpanStatus::Unset => json!({ "code": 0 }),
        SpanStatus::Ok => json!({ "code": 1 }),
        SpanStatus::Error(msg) => json!({ "code": 2, "message": msg.as_str() }),
    }
}

/// An OTLP `KeyValue`. `intValue` is a string per the JSON encoding.
fn attr_kv(key: &str, value: &AttrValue) -> Value {
    let v = match value {
        AttrValue::Str(s) => json!({ "stringValue": s.as_str() }),
        AttrValue::Int(i) => json!({ "intValue": i.to_string() }),
        AttrValue::Float(f) => json!({ "doubleValue": f }),
        AttrValue::Bool(b) => json!({ "boolValue": b }),
    };
    json!({ "key": key, "value": v })
}

fn kind_code(kind: SpanKind) -> u8 {
    match kind {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    }
}

/// Milliseconds → the OTLP nanosecond timestamp, rendered as a string (uint64 JSON encoding).
fn nanos(unix_ms: u64) -> String {
    (unix_ms.saturating_mul(1_000_000)).to_string()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_stdlib::{SpanEvent, TraceContext};

    fn ctx(trace: u8, span: u8) -> TraceContext {
        TraceContext {
            trace_id: [trace; 16],
            span_id: [span; 8],
            sampled: true,
        }
    }

    #[test]
    fn otlp_json_shape_is_valid() {
        let span = SpanData {
            name: "handle_request".into(),
            kind: SpanKind::Server,
            context: ctx(0xab, 0xcd),
            parent: Some(ctx(0xab, 0x11)),
            start_unix_ms: 1_000,
            end_unix_ms: Some(1_050),
            attributes: vec![
                ("http.method".into(), AttrValue::Str("GET".into())),
                ("http.status".into(), AttrValue::Int(200)),
                ("ok".into(), AttrValue::Bool(true)),
            ],
            events: vec![SpanEvent {
                name: "cache.miss".into(),
                unix_ms: 1_010,
                attributes: vec![],
            }],
            status: SpanStatus::Error("boom".into()),
        };
        let body = spans_to_json(&[span], "svc");
        let rs = &body["resourceSpans"][0];
        assert_eq!(
            rs["resource"]["attributes"][0]["value"]["stringValue"],
            "svc"
        );
        let s = &rs["scopeSpans"][0]["spans"][0];
        assert_eq!(s["name"], "handle_request");
        assert_eq!(s["kind"], 2); // SERVER
        assert_eq!(s["traceId"], "abababababababababababababababab");
        assert_eq!(s["spanId"], "cdcdcdcdcdcdcdcd");
        assert_eq!(s["parentSpanId"], "1111111111111111");
        // int64/uint64 fields are strings
        assert_eq!(s["startTimeUnixNano"], "1000000000");
        assert_eq!(s["endTimeUnixNano"], "1050000000");
        assert_eq!(s["attributes"][1]["value"]["intValue"], "200");
        assert_eq!(s["attributes"][2]["value"]["boolValue"], true);
        assert_eq!(s["status"]["code"], 2);
        assert_eq!(s["status"]["message"], "boom");
        assert_eq!(s["events"][0]["name"], "cache.miss");
    }

    #[test]
    fn root_span_has_no_parent() {
        let span = SpanData {
            name: "root".into(),
            kind: SpanKind::Internal,
            context: ctx(1, 2),
            parent: None,
            start_unix_ms: 0,
            end_unix_ms: Some(0),
            attributes: vec![],
            events: vec![],
            status: SpanStatus::Ok,
        };
        let body = spans_to_json(&[span], "noeta");
        let s = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(s.get("parentSpanId").is_none());
        assert_eq!(s["status"]["code"], 1);
    }

    #[test]
    fn otlp_logs_json_shape_is_valid() {
        let records = vec![
            LogRecord {
                unix_ms: 2_000,
                severity: Severity::Info,
                body: "started".into(),
                attributes: vec![("worker".into(), AttrValue::Int(3))],
                trace_context: Some(ctx(0xab, 0xcd)),
            },
            LogRecord {
                unix_ms: 2_500,
                severity: Severity::Error,
                body: "boom".into(),
                attributes: vec![],
                trace_context: None,
            },
        ];
        let body = logs_to_json(&records, "svc");
        let rl = &body["resourceLogs"][0];
        assert_eq!(rl["resource"]["attributes"][0]["value"]["stringValue"], "svc");
        let recs = &rl["scopeLogs"][0]["logRecords"];
        // Correlated INFO record.
        assert_eq!(recs[0]["severityNumber"], 9);
        assert_eq!(recs[0]["severityText"], "INFO");
        assert_eq!(recs[0]["body"]["stringValue"], "started");
        assert_eq!(recs[0]["timeUnixNano"], "2000000000");
        assert_eq!(recs[0]["attributes"][0]["value"]["intValue"], "3");
        assert_eq!(recs[0]["traceId"], "abababababababababababababababab");
        assert_eq!(recs[0]["spanId"], "cdcdcdcdcdcdcdcd");
        // Top-level ERROR record — no trace correlation.
        assert_eq!(recs[1]["severityNumber"], 17);
        assert_eq!(recs[1]["severityText"], "ERROR");
        assert!(recs[1].get("traceId").is_none());
        assert!(recs[1].get("spanId").is_none());
    }

    #[test]
    fn header_parse_skips_malformed() {
        let h = parse_headers("authorization=Bearer x,,bad,x-tenant = t2");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], ("authorization".to_string(), "Bearer x".to_string()));
        assert_eq!(h[1], ("x-tenant".to_string(), "t2".to_string()));
    }
}
