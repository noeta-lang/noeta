//! Native OTEL T4 — server auto-instrumentation, the telemetry conformance oracle.
//!
//! `http.serve` wraps each accepted connection in a SERVER-kind span. That span is a *write-only
//! side effect* — invisible to program output, so the differential can't see it — so this test
//! observes the spans directly: it runs a served program on a [`SandboxHost`] whose recorder feeds a
//! shared sink ([`SandboxHost::set_span_sink`]) that outlives the host (the VM consumes it), then
//! asserts on the emitted spans. Under the sandbox the accept leaf drives the fixed five-request
//! script (`GET /`, `GET /health`, `POST /echo`, `GET /users/42?active=true`, `DELETE /users/42`),
//! so the emitted spans are deterministic.

use std::sync::{Arc, Mutex};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::{AttrValue, SandboxHost, SpanData, SpanKind, SpanStatus};
use noeta_vm::VmBackend;

/// Compile a single-file program to a runnable module (panicking on any front-end diagnostic — the
/// test programs are known-good).
fn compile(text: &str) -> noeta_bytecode::Module {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "telemetry_serve.noe", text);
    let src = noeta_db::source_program(&db, &source);
    assert!(
        noeta_db::tokens(&db, src).0.diagnostics.is_empty()
            && noeta_db::ast(&db, src).0.diagnostics.is_empty(),
        "program parses cleanly"
    );
    assert!(
        noeta_db::checked(&db, src).diagnostics.is_empty(),
        "program type-checks"
    );
    noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("program compiles")
        .clone()
}

/// Run `text` on a sandbox host with a span sink installed, returning the spans it emitted (in end
/// order). The host is moved into the VM and dropped at teardown, so the sink — not the host — is
/// what survives the run.
fn emitted_spans(text: &str) -> Vec<SpanData> {
    let module = compile(text);
    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut host = SandboxHost::new();
    host.set_span_sink(sink.clone());
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    assert!(
        result.is_ok(),
        "program ran cleanly (exit {}): {}",
        result.exit_code,
        result.stdout
    );
    // The sink survives the dropped host; return its accumulated spans.
    let guard = sink.lock().unwrap();
    guard.clone()
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a AttrValue> {
    span.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Every accepted connection produces one SERVER span, named `"{method} {route}"` with the query
/// stripped, in the sandbox script's order — the headline of T4.
#[test]
fn serve_emits_one_server_span_per_request() {
    let spans = emitted_spans(
        "use std.{http}\n\
         fn fetch(req: Request): Response { return http.response(200, \"ok\") }\n\
         http.serve(8080, fetch)\n",
    );

    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["GET /", "GET /health", "POST /echo", "GET /users/42", "DELETE /users/42"]
    );
    assert!(
        spans.iter().all(|s| s.kind == SpanKind::Server),
        "every request span is SERVER-kind"
    );
    // No inbound `traceparent` in the script → every span is a fresh root.
    assert!(spans.iter().all(|s| s.parent.is_none()), "roots (no inbound parent)");
    assert!(spans.iter().all(|s| s.end_unix_ms.is_some()), "all ended");
}

/// The HTTP semantic-convention attributes ride the span: request method + path, and the response
/// status code recorded at end.
#[test]
fn serve_span_carries_http_attributes() {
    let spans = emitted_spans(
        "use std.{http}\n\
         fn fetch(req: Request): Response { return http.response(201, \"made\") }\n\
         http.serve(8080, fetch)\n",
    );
    let echo = &spans[2]; // POST /echo
    assert_eq!(
        attr(echo, "http.request.method"),
        Some(&AttrValue::Str("POST".into()))
    );
    assert_eq!(attr(echo, "url.path"), Some(&AttrValue::Str("/echo".into())));
    assert_eq!(
        attr(echo, "http.response.status_code"),
        Some(&AttrValue::Int(201))
    );
}

/// A `5xx` reply marks its span an error (OTel HTTP convention); a `2xx`/`4xx` leaves it unset. Here
/// the handler answers `/health` with `503` and everything else `200`.
#[test]
fn serve_span_status_reflects_5xx_only() {
    let spans = emitted_spans(
        "use std.{http}\n\
         fn fetch(req: Request): Response {\n\
         \x20   if req.path() == \"/health\" { return http.response(503, \"down\") }\n\
         \x20   return http.response(200, \"ok\")\n\
         }\n\
         http.serve(8080, fetch)\n",
    );
    // span[1] is `GET /health` → 503 → Error; the rest are 200 → Unset.
    assert_eq!(spans[1].name, "GET /health");
    assert_eq!(spans[1].status, SpanStatus::Error("HTTP 503".into()));
    assert!(
        spans
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .all(|(_, s)| s.status == SpanStatus::Unset),
        "only the 5xx span is an error"
    );
}
