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
/// what survives the run. Also runs the SAME program on the tree-walker with its own sink and
/// asserts the recorded spans are **identical** — both backends drive the same cooperative schedule
/// against the same deterministic recorder, so parenting parity is exact, not merely structural.
/// (Spans are invisible to `RunResult`, so the ordinary differential cannot see them; this is the
/// telemetry twin of that oracle.)
fn emitted_spans(text: &str) -> Vec<SpanData> {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "telemetry_serve.noe", text);
    let src = noeta_db::source_program(&db, &source);
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
    let vm_spans: Vec<SpanData> = sink.lock().unwrap().clone();

    // The tree-walker twin: same program, its own sandbox + sink.
    let program = noeta_db::ast(&db, src).0.program.clone();
    let sites = noeta_db::checked(&db, src).sites.clone();
    let tree_sink = Arc::new(Mutex::new(Vec::new()));
    let mut tree_host = SandboxHost::new();
    tree_host.set_span_sink(tree_sink.clone());
    let tree_result = noeta_conformance::reference::reference_run_with_host(
        &program,
        sites,
        Box::new(tree_host),
    );
    assert!(tree_result.is_ok(), "tree-walker ran cleanly");
    let tree_spans: Vec<SpanData> = tree_sink.lock().unwrap().clone();
    assert_eq!(
        vm_spans, tree_spans,
        "both backends record identical spans (telemetry parity)"
    );

    vm_spans
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

/// T5b — a handler's own `with_span` nests under its request's SERVER span: the handler runs under
/// a task-local context seeded with that span, so the child's parent IS the server span's context
/// (same trace, parent span id = server span id) — one connected trace per request.
#[test]
fn handler_spans_nest_under_the_server_span() {
    let spans = emitted_spans(
        "use std.{http, telemetry}\n\
         fn fetch(req: Request): Response {\n\
         \x20   body = fn(): int { return 1 }\n\
         \x20   telemetry.with_span(\"db\", body)\n\
         \x20   return http.response(200, \"ok\")\n\
         }\n\
         http.serve(8080, fetch)\n",
    );
    let db: Vec<_> = spans.iter().filter(|s| s.name == "db").collect();
    let servers: Vec<_> = spans.iter().filter(|s| s.kind == SpanKind::Server).collect();
    assert_eq!(db.len(), 5, "one child span per scripted request");
    assert_eq!(servers.len(), 5);
    for (child, server) in db.iter().zip(&servers) {
        let parent = child.parent.expect("child has a parent");
        assert_eq!(parent, server.context, "child parents under ITS request's server span");
        assert_eq!(child.context.trace_id, server.context.trace_id, "same trace");
    }
}

/// T5b isolation — five *interleaved* async handlers (all suspend at a sleep before creating their
/// span) must each parent under their OWN request's SERVER span. With one global active-span stack
/// this cross-parented; per-handler contexts (swapped in around every poll) keep the five traces
/// disjoint and 1:1.
#[test]
fn interleaved_handlers_keep_their_own_context() {
    let spans = emitted_spans(
        "use std.{http, telemetry}\n\
         use std.task.{sleep}\n\
         async fn fetch(req: Request): Response {\n\
         \x20   sleep(5).await\n\
         \x20   body = fn(): int { return 1 }\n\
         \x20   telemetry.with_span(\"work\", body)\n\
         \x20   return http.response(200, \"ok\")\n\
         }\n\
         http.serve(8080, fetch)\n",
    );
    let work: Vec<_> = spans.iter().filter(|s| s.name == "work").collect();
    let servers: Vec<_> = spans.iter().filter(|s| s.kind == SpanKind::Server).collect();
    assert_eq!(work.len(), 5);
    assert_eq!(servers.len(), 5);
    // Every work span parents under exactly one distinct server span (a bijection), and shares its
    // trace — no cross-request leakage.
    let mut claimed: Vec<[u8; 8]> = Vec::new();
    for w in &work {
        let parent = w.parent.expect("work has a parent");
        let server = servers
            .iter()
            .find(|s| s.context.span_id == parent.span_id)
            .expect("parent is one of the server spans");
        assert_eq!(w.context.trace_id, server.context.trace_id, "stays in its own trace");
        assert!(
            !claimed.contains(&parent.span_id),
            "two handlers parented under the same request"
        );
        claimed.push(parent.span_id);
    }
}

/// T5b inheritance — a task `spawn`ed inside a handler snapshots the handler's context, so a span it
/// creates on its own strand still parents under that request's SERVER span.
#[test]
fn handler_spawned_task_inherits_the_server_span() {
    let spans = emitted_spans(
        "use std.{http, telemetry}\n\
         async fn bg(): int {\n\
         \x20   s = telemetry.span(\"bg\")\n\
         \x20   s.end()\n\
         \x20   return 1\n\
         }\n\
         async fn fetch(req: Request): Response {\n\
         \x20   mut done = 0\n\
         \x20   concurrent {\n\
         \x20       h = spawn bg()\n\
         \x20       done = h.await\n\
         \x20   }\n\
         \x20   return http.response(200, \"ok\")\n\
         }\n\
         http.serve(8080, fetch)\n",
    );
    let bg: Vec<_> = spans.iter().filter(|s| s.name == "bg").collect();
    let servers: Vec<_> = spans.iter().filter(|s| s.kind == SpanKind::Server).collect();
    assert_eq!(bg.len(), 5);
    assert_eq!(servers.len(), 5);
    for (child, server) in bg.iter().zip(&servers) {
        let parent = child.parent.expect("bg has a parent");
        assert_eq!(parent, server.context, "inherited across the spawn boundary");
    }
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
