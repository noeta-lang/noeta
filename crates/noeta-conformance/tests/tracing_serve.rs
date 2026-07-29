//! Native OTEL T4 — server auto-instrumentation, the telemetry conformance oracle.
//!
//! `http.serve` wraps each accepted connection in a SERVER-kind span. That span is a *write-only
//! side effect* — invisible to program output, so the differential can't see it — so this test
//! observes the spans directly: it runs a served program on a [`SandboxHost`] whose recorder feeds a
//! shared sink ([`SandboxHost::set_span_sink`]) that outlives the host (the VM consumes it), then
//! asserts on the emitted spans. Under the sandbox the accept leaf drives a fixed request script,
//! so the emitted spans are deterministic. Every count below is DERIVED from
//! `sandbox_request_script` — the one place the script is defined — so a request added there
//! updates these tests instead of rotting them, and the expected span names are derived from the
//! script itself rather than transcribed.

use std::sync::{Arc, Mutex};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::{AttrValue, SandboxHost, SpanData, SpanKind, SpanStatus};
use noeta_vm::VmBackend;

/// Compile a single-file program to a runnable module (panicking on any front-end diagnostic — the
/// test programs are known-good).
fn compile(text: &str) -> noeta_bytecode::Module {
    noeta_conformance::ensure_std_registry();
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "telemetry_serve.noe", text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
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
    let (spans, ok) = emitted_spans_any(text);
    assert!(ok, "program ran cleanly");
    spans
}

/// [`emitted_spans`] without the clean-exit requirement (for abort-path programs): returns the
/// recorded spans plus whether the run succeeded. Backend parity is still asserted — both the
/// spans AND the exit disposition must agree.
fn emitted_spans_any(text: &str) -> (Vec<SpanData>, bool) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "telemetry_serve.noe", text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let module = compile(text);

    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut host = SandboxHost::new();
    host.set_span_sink(sink.clone());
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    let vm_spans: Vec<SpanData> = sink.lock().unwrap().clone();

    // The tree-walker twin: same program, its own sandbox + sink.
    let program = noeta_db::ast(&db, src).0.program.clone();
    let sites = noeta_db::checked(&db, src).sites.clone();
    let tree_sink = Arc::new(Mutex::new(Vec::new()));
    let mut tree_host = SandboxHost::new();
    tree_host.set_span_sink(tree_sink.clone());
    let tree_result =
        noeta_conformance::reference::reference_run_with_host(&program, sites, Box::new(tree_host));
    assert_eq!(
        result.is_ok(),
        tree_result.is_ok(),
        "both backends agree on the exit disposition"
    );
    let tree_spans: Vec<SpanData> = tree_sink.lock().unwrap().clone();
    assert_eq!(
        vm_spans, tree_spans,
        "both backends record identical spans (telemetry parity)"
    );

    (vm_spans, result.is_ok())
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a AttrValue> {
    span.attributes
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// How many requests the sandbox's inbound script drives — the single source every count in this
/// file derives from, so adding a scripted request never leaves a stale integer behind.
fn scripted() -> usize {
    noeta_stdlib::net::sandbox_request_script().len()
}

/// The SERVER-span names the script must produce, derived from the script itself: OTel names a
/// server span `"{method} {route}"`, where the route is the path with any query stripped (a raw
/// query would explode span cardinality). Derived rather than transcribed so the expectation and
/// the script cannot disagree about what was sent.
fn expected_span_names() -> Vec<String> {
    noeta_stdlib::net::sandbox_request_script()
        .iter()
        .map(|r| {
            let path = r.url.split(['?', '#']).next().unwrap_or(&r.url);
            format!("{} {path}", r.method)
        })
        .collect()
}

/// The one span with `name`, failing loudly if it is absent or ambiguous.
///
/// Every assertion that used to index the span list by position goes through this instead: an
/// index silently starts checking a *different* request when the script grows, which is a test
/// that keeps passing while measuring the wrong thing.
fn span_named<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let mut matching = spans.iter().filter(|s| s.name == name);
    let found = matching
        .next()
        .unwrap_or_else(|| panic!("no span named `{name}` among {:?}", names_of(spans)));
    assert!(
        matching.next().is_none(),
        "several spans named `{name}` — the assertion would be ambiguous"
    );
    found
}

/// The span names, for a failure message.
fn names_of(spans: &[SpanData]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_str()).collect()
}

/// Every accepted connection produces one SERVER span, named `"{method} {route}"` with the query
/// stripped, in the sandbox script's order — the headline of T4.
#[test]
fn serve_emits_one_server_span_per_request() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         fn fetch(req: Request): Response { return server.response(200, \"ok\") }\n\
         server.serve(8080, fetch)\n",
    );

    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        expected_span_names(),
        "one span per scripted request, in script order"
    );
    assert!(
        spans.iter().all(|s| s.kind == SpanKind::Server),
        "every request span is SERVER-kind"
    );
    // No inbound `traceparent` in the script → every span is a fresh root.
    assert!(
        spans.iter().all(|s| s.parent.is_none()),
        "roots (no inbound parent)"
    );
    assert!(spans.iter().all(|s| s.end_unix_ms.is_some()), "all ended");
}

/// The HTTP semantic-convention attributes ride the span: request method + path, and the response
/// status code recorded at end.
#[test]
fn serve_span_carries_http_attributes() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         fn fetch(req: Request): Response { return server.response(201, \"made\") }\n\
         server.serve(8080, fetch)\n",
    );
    // Located by NAME, not by position: an index into the script is exactly the kind of
    // assertion that silently starts checking a different request when the script grows.
    let echo = span_named(&spans, "POST /echo");
    assert_eq!(
        attr(echo, "http.request.method"),
        Some(&AttrValue::Str("POST".into()))
    );
    assert_eq!(
        attr(echo, "url.path"),
        Some(&AttrValue::Str("/echo".into()))
    );
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
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         use std.{tracing}\n\
         fn fetch(req: Request): Response {\n\
         \x20   body = fn(): int { return 1 }\n\
         \x20   tracing.with_span(\"db\", body)\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );
    let db: Vec<_> = spans.iter().filter(|s| s.name == "db").collect();
    let servers: Vec<_> = spans
        .iter()
        .filter(|s| s.kind == SpanKind::Server)
        .collect();
    assert_eq!(db.len(), scripted(), "one child span per scripted request");
    assert_eq!(servers.len(), scripted());
    for (child, server) in db.iter().zip(&servers) {
        let parent = child.parent.expect("child has a parent");
        assert_eq!(
            parent, server.context,
            "child parents under ITS request's server span"
        );
        assert_eq!(
            child.context.trace_id, server.context.trace_id,
            "same trace"
        );
    }
}

/// T5b isolation — five *interleaved* async handlers (all suspend at a sleep before creating their
/// span) must each parent under their OWN request's SERVER span. With one global active-span stack
/// this cross-parented; per-handler contexts (swapped in around every poll) keep the five traces
/// disjoint and 1:1.
#[test]
fn interleaved_handlers_keep_their_own_context() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         use std.{tracing}\n\
         use std.task.{sleep}\n\
         async fn fetch(req: Request): Response {\n\
         \x20   sleep(5).await\n\
         \x20   body = fn(): int { return 1 }\n\
         \x20   tracing.with_span(\"work\", body)\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );
    let work: Vec<_> = spans.iter().filter(|s| s.name == "work").collect();
    let servers: Vec<_> = spans
        .iter()
        .filter(|s| s.kind == SpanKind::Server)
        .collect();
    assert_eq!(work.len(), scripted());
    assert_eq!(servers.len(), scripted());
    // Every work span parents under exactly one distinct server span (a bijection), and shares its
    // trace — no cross-request leakage.
    let mut claimed: Vec<[u8; 8]> = Vec::new();
    for w in &work {
        let parent = w.parent.expect("work has a parent");
        let server = servers
            .iter()
            .find(|s| s.context.span_id == parent.span_id)
            .expect("parent is one of the server spans");
        assert_eq!(
            w.context.trace_id, server.context.trace_id,
            "stays in its own trace"
        );
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
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         use std.{tracing}\n\
         async fn bg(): int {\n\
         \x20   s = tracing.span(\"bg\")\n\
         \x20   s.end()\n\
         \x20   return 1\n\
         }\n\
         async fn fetch(req: Request): Response {\n\
         \x20   mut done = 0\n\
         \x20   concurrent {\n\
         \x20       h = spawn bg()\n\
         \x20       done = h.await\n\
         \x20   }\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );
    let bg: Vec<_> = spans.iter().filter(|s| s.name == "bg").collect();
    let servers: Vec<_> = spans
        .iter()
        .filter(|s| s.kind == SpanKind::Server)
        .collect();
    assert_eq!(bg.len(), scripted());
    assert_eq!(servers.len(), scripted());
    for (child, server) in bg.iter().zip(&servers) {
        let parent = child.parent.expect("bg has a parent");
        assert_eq!(
            parent, server.context,
            "inherited across the spawn boundary"
        );
    }
}

/// T5c — `with_span` over an ASYNC body has the body's duration, not the construction's: the
/// future-completion hook ends the span when the future resolves, so the sleep inside the body is
/// inside the span. And a span created AFTER the suspension still nests under it — the traced
/// context follows the future across polls.
#[test]
fn with_span_async_covers_the_bodys_duration() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         use std.task.{sleep}\n\
         async fn work(): int {\n\
         \x20   early = tracing.span(\"early\")\n\
         \x20   early.end()\n\
         \x20   sleep(5).await\n\
         \x20   late = tracing.span(\"late\")\n\
         \x20   late.end()\n\
         \x20   return 1\n\
         }\n\
         echo tracing.with_span(\"job\", work).await\n",
    );
    let job = spans
        .iter()
        .find(|s| s.name == "job")
        .expect("job span recorded");
    assert!(job.parent.is_none(), "top-level span is a root");
    assert!(job.end_unix_ms.is_some(), "ended at completion");
    // End-at-completion, pinned by RECORDING ORDER (the recorder appends at `end`): the job span
    // must end after the post-suspension child — with the old end-at-construction behavior it was
    // recorded first, before the body ever ran. (The sandbox host's logical wall clock does not
    // advance with executor timers, so duration itself reads 0 here; the real host shares one
    // clock and gets true durations.)
    let pos = |name: &str| spans.iter().position(|s| s.name == name).unwrap();
    assert!(pos("early") < pos("late"), "children end in body order");
    assert!(
        pos("late") < pos("job"),
        "job ends after the body completes"
    );
    // Both children — before AND after the suspension — parent under the job span.
    for name in ["early", "late"] {
        let child = spans.iter().find(|s| s.name == name).unwrap();
        assert_eq!(child.parent, Some(job.context), "{name} nests under job");
    }
}

/// T5c — an async body that ABORTS after suspending still ends its span, with the error status,
/// exactly like the sync abort path (the completion hook's abort arm).
#[test]
fn with_span_async_abort_ends_the_span_with_error() {
    let (spans, ok) = emitted_spans_any(
        "use std.{tracing}\n\
         use std.task.{sleep}\n\
         async fn boom(): int {\n\
         \x20   sleep(2).await\n\
         \x20   panic(\"nope\")\n\
         }\n\
         echo tracing.with_span(\"job\", boom).await\n",
    );
    assert!(!ok, "the abort propagates to the program");
    let job = spans
        .iter()
        .find(|s| s.name == "job")
        .expect("aborted span still recorded");
    assert_eq!(job.status, SpanStatus::Error("span body aborted".into()));
    assert!(job.end_unix_ms.is_some(), "ended despite the abort");
}

/// T5d — automatic channel propagation, end to end: the producer sends inside its span (context
/// rides the envelope), the consumer — spawned with an EMPTY context — is seeded on recv, and a
/// real span it then creates parents under the *producer's* span, across strands, with zero user
/// threading. The message type stays `int`.
#[test]
fn channel_seeded_consumer_spans_parent_under_the_producer() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         (tx, rx) = channel::<int>(1)\n\
         async fn produce() use (tx): int {\n\
         \x20   tx.send(7).await\n\
         \x20   tx.close()\n\
         \x20   return 0\n\
         }\n\
         async fn consume() use (rx): int {\n\
         \x20   r = rx.recv().await\n\
         \x20   work = tracing.span(\"work\")\n\
         \x20   work.end()\n\
         \x20   return match r { some(x) => x, none => 0 }\n\
         }\n\
         async fn run(): int {\n\
         \x20   mut got = 0\n\
         \x20   concurrent {\n\
         \x20       h = spawn consume()\n\
         \x20       tracing.with_span(\"produce\", produce).await\n\
         \x20       got = h.await\n\
         \x20   }\n\
         \x20   return got\n\
         }\n\
         echo run().await\n",
    );
    let produce = spans.iter().find(|s| s.name == "produce").unwrap();
    let work = spans.iter().find(|s| s.name == "work").unwrap();
    let parent = work
        .parent
        .expect("the seeded consumer's span has a parent");
    assert_eq!(
        parent, produce.context,
        "the consumer's span parents under the producer's, across the channel"
    );
    assert_eq!(work.context.trace_id, produce.context.trace_id, "one trace");
}

/// A `5xx` reply marks its span an error (OTel HTTP convention); a `2xx`/`4xx` leaves it unset. Here
/// the handler answers `/health` with `503` and everything else `200`.
#[test]
fn serve_span_status_reflects_5xx_only() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         fn fetch(req: Request): Response {\n\
         \x20   if req.path() == \"/health\" { return server.response(503, \"down\") }\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );
    // The `GET /health` span is the 503 → Error; every other span is 200 → Unset. Both halves key
    // on the NAME rather than a script index, so this keeps checking the health request whatever
    // else the script grows.
    let health = span_named(&spans, "GET /health");
    assert_eq!(health.status, SpanStatus::Error("HTTP 503".into()));
    assert!(
        spans
            .iter()
            .filter(|s| s.name != "GET /health")
            .all(|s| s.status == SpanStatus::Unset),
        "only the 5xx span is an error"
    );
}

/// L4 (native-otel T5e) — opt-in reactive-flush telemetry. With `NOETA_TRACE_REACTIVE` set (the
/// program's own env view suffices), every non-empty flush emits a `reactive.flush` span whose
/// attributes count the effects run and the distinct changed nodes, a `view.diff` emits its
/// dirty-inspected/actually-pushed counts, and a span created *inside* an effect body parents
/// under its flush span — reactive propagation joins the trace. Backend parity is asserted by
/// the harness (byte-identical spans).
#[test]
fn reactive_flush_spans_are_opt_in_and_carry_propagation_counts() {
    let spans = emitted_spans(
        "use std.{env, tracing}\n\
         use std.reactive.{signal, computed, effect, view}\n\
         env.set(\"NOETA_TRACE_REACTIVE\", \"1\")\n\
         count = signal(1)\n\
         double = computed(fn() { return count.get() * 2 })\n\
         v = view()\n\
         v.expose(\"count\", count)\n\
         v.expose(\"double\", double)\n\
         effect(fn() {\n\
         \x20   tracing.with_span(\"inside\", fn(): void {})\n\
         \x20   count.get()\n\
         })\n\
         count.set(2)\n\
         v.diff()\n",
    );

    let flushes: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "reactive.flush")
        .collect();
    assert_eq!(
        flushes.len(),
        2,
        "one span per non-empty flush: effect creation + the set"
    );
    for f in &flushes {
        assert_eq!(
            attr(f, "reactive.effects"),
            Some(&AttrValue::Int(1)),
            "each flush ran the one effect"
        );
    }
    assert_eq!(
        attr(flushes[0], "reactive.changed"),
        Some(&AttrValue::Int(0)),
        "the creation flush changed nothing"
    );
    assert_eq!(
        attr(flushes[1], "reactive.changed"),
        Some(&AttrValue::Int(2)),
        "the set changed the signal and dirtied its computed"
    );

    // The effect body's own span parents under the flush that ran it (both runs of the effect).
    let insides: Vec<_> = spans.iter().filter(|s| s.name == "inside").collect();
    assert_eq!(
        insides.len(),
        2,
        "the effect ran at creation and at the set"
    );
    for (inside, flush) in insides.iter().zip(&flushes) {
        assert_eq!(
            inside.parent.as_ref().map(|c| c.span_id),
            Some(flush.context.span_id),
            "an effect-body span nests under its flush"
        );
    }

    let diffs: Vec<_> = spans.iter().filter(|s| s.name == "view.diff").collect();
    assert_eq!(diffs.len(), 1);
    assert_eq!(attr(diffs[0], "view.dirty"), Some(&AttrValue::Int(2)));
    assert_eq!(attr(diffs[0], "view.pushed"), Some(&AttrValue::Int(2)));
}

/// The flag off (the default): the same reactive program emits NO reactive/view spans — per-flush
/// tracing is opt-in, and the off path is one cached-bool branch.
#[test]
fn reactive_flush_spans_are_absent_without_the_flag() {
    let spans = emitted_spans(
        "use std.reactive.{signal, effect, view}\n\
         count = signal(1)\n\
         v = view()\n\
         v.expose(\"count\", count)\n\
         effect(fn() { count.get() })\n\
         count.set(2)\n\
         v.diff()\n",
    );
    assert!(
        spans
            .iter()
            .all(|s| s.name != "reactive.flush" && s.name != "view.diff"),
        "no reactive spans without NOETA_TRACE_REACTIVE: {spans:?}"
    );
}

// ----- the active-span annotators -----------------------------------------------------------
//
// `tracing.set_attribute`/`add_event`/`record_error` apply the `Span` mutations to the span the
// program is already INSIDE. Their `bool` return (did a live active span receive it) is ordinary
// program output and is pinned by the conformance corpus (`tests/conformance/tracing/active_span_*`);
// WHICH span received each annotation is only visible here, against the recorder.

/// Whether `span` carries an event named `name`.
fn has_event(span: &SpanData, name: &str) -> bool {
    span.events.iter().any(|e| e.name == name)
}

/// The headline: annotations inside a `with_span` body land on THAT span — no child span is created
/// to carry them. Before this surface the only way to record "a thing happened here" from inside a
/// body was a short child span, which is a span where an annotation belongs.
#[test]
fn active_annotations_land_on_the_enclosing_span() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         body = fn(): void {\n\
         \x20   tracing.set_attribute(\"guardrail.verdict\", \"allow\")\n\
         \x20   tracing.add_event(\"guardrail.checked\")\n\
         \x20   tracing.record_error(\"policy violation\")\n\
         }\n\
         tracing.with_span(\"run\", body)\n",
    );

    assert_eq!(
        names_of(&spans),
        vec!["run"],
        "exactly one span — the annotations did NOT mint a child"
    );
    let run = span_named(&spans, "run");
    assert_eq!(
        attr(run, "guardrail.verdict"),
        Some(&AttrValue::Str("allow".into())),
        "the attribute reached the enclosing span"
    );
    assert!(has_event(run, "guardrail.checked"), "the event reached it");
    assert_eq!(
        run.status,
        SpanStatus::Error("policy violation".into()),
        "record_error set the enclosing span's status"
    );
}

/// `add_event_with` carries the event's OWN attributes, so several structured facts recorded on one
/// span each keep their own set — where span-level `set_attribute` would have them overwrite each
/// other by key. This is the shape a per-verdict / per-retry record actually needs, and it is why
/// consumers reached for a short child span instead: a bare `add_event(name)` could not carry the
/// reason, and a span attribute could only hold the last one.
#[test]
fn active_events_carry_their_own_attributes() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         body = fn(): void {\n\
         \x20   tracing.add_event_with(\"guard.denied\", {\"guard\": \"pii\", \"reason\": \"email\"})\n\
         \x20   tracing.add_event_with(\"guard.denied\", {\"guard\": \"secrets\", \"reason\": \"key\"})\n\
         }\n\
         tracing.with_span(\"run\", body)\n",
    );

    let run = span_named(&spans, "run");
    assert_eq!(names_of(&spans), vec!["run"], "no child spans minted");
    let denied: Vec<_> = run
        .events
        .iter()
        .filter(|e| e.name == "guard.denied")
        .collect();
    assert_eq!(denied.len(), 2, "events accumulate; attributes would not");
    let guards: Vec<&AttrValue> = denied
        .iter()
        .filter_map(|e| e.attributes.iter().find(|(k, _)| k == "guard"))
        .map(|(_, v)| v)
        .collect();
    assert_eq!(
        guards,
        vec![
            &AttrValue::Str("pii".into()),
            &AttrValue::Str("secrets".into())
        ],
        "each event kept its own attribute set"
    );
    assert!(
        run.attributes.is_empty(),
        "an event's attributes do not leak onto the span"
    );
}

/// The `Span` handle gained the same structured event, so the two receivers stay symmetric — a span
/// you hold can record exactly what the active-span surface can.
#[test]
fn held_span_events_carry_their_own_attributes() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         s = tracing.span(\"db.lookup\")\n\
         s.add_event_with(\"retry\", {\"attempt\": 2, \"backoff.ms\": 50.5, \"final\": true}).end()\n",
    );
    let span = span_named(&spans, "db.lookup");
    let retry = span
        .events
        .iter()
        .find(|e| e.name == "retry")
        .expect("retry event recorded");
    // The whole scalar union round-trips through the deep-marshalled map argument.
    assert_eq!(
        retry.attributes,
        vec![
            ("attempt".into(), AttrValue::Int(2)),
            ("backoff.ms".into(), AttrValue::Float(50.5)),
            ("final".into(), AttrValue::Bool(true)),
        ]
    );
}

/// Nesting: the active span is always the INNERMOST one, and the outer span becomes active again
/// after the inner body returns (the pop restores it) — so a later annotation lands on the outer
/// span and not on the already-ended inner one.
#[test]
fn active_annotations_target_the_innermost_span() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         inner_body = fn(): void {\n\
         \x20   tracing.set_attribute(\"depth\", 2)\n\
         \x20   tracing.add_event(\"inner.step\")\n\
         }\n\
         outer_body = fn(): void {\n\
         \x20   tracing.set_attribute(\"depth\", 1)\n\
         \x20   tracing.with_span(\"inner\", inner_body)\n\
         \x20   tracing.add_event(\"outer.after\")\n\
         }\n\
         tracing.with_span(\"outer\", outer_body)\n",
    );

    let inner = span_named(&spans, "inner");
    let outer = span_named(&spans, "outer");
    assert_eq!(attr(inner, "depth"), Some(&AttrValue::Int(2)));
    assert_eq!(
        attr(outer, "depth"),
        Some(&AttrValue::Int(1)),
        "the inner body's attribute did not overwrite the outer span's"
    );
    assert!(has_event(inner, "inner.step"));
    assert!(!has_event(inner, "outer.after"));
    assert!(
        has_event(outer, "outer.after"),
        "after the inner body returns, the OUTER span is active again"
    );
    assert!(!has_event(outer, "inner.step"));
    assert_eq!(
        inner.parent.map(|c| c.span_id),
        Some(outer.context.span_id),
        "still ordinary nesting"
    );
}

/// The case the consumer actually wanted: a request handler runs under the auto-instrumented SERVER
/// span, and an annotation from inside the handler reaches THAT span — no handle exists for it, and
/// before this surface the only way to record a per-request fact was a child span per fact.
#[test]
fn handler_annotates_the_server_span_itself() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         use std.{tracing}\n\
         fn fetch(req: Request): Response {\n\
         \x20   tracing.set_attribute(\"guardrail.verdict\", \"allow\")\n\
         \x20   tracing.add_event(\"guardrail.checked\")\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );

    let servers: Vec<_> = spans
        .iter()
        .filter(|s| s.kind == SpanKind::Server)
        .collect();
    assert_eq!(servers.len(), scripted());
    // The whole point: the annotations rode the SERVER spans, and NO extra span was minted for
    // them. One span per request, richer — not one span per request plus one per annotation.
    assert_eq!(
        spans.len(),
        scripted(),
        "no child spans were created to carry the annotations: {:?}",
        names_of(&spans)
    );
    for server in &servers {
        assert_eq!(
            attr(server, "guardrail.verdict"),
            Some(&AttrValue::Str("allow".into())),
            "the handler annotated its own request's SERVER span"
        );
        assert!(has_event(server, "guardrail.checked"));
        // The auto-instrumented attributes are untouched — the handler added to the span, it did
        // not replace what `http.serve` records.
        assert!(attr(server, "url.path").is_some());
        assert_eq!(
            attr(server, "http.response.status_code"),
            Some(&AttrValue::Int(200)),
            "http.serve still ended the span itself — the handler cannot end it"
        );
    }
}

/// `record_error` from a handler marks the request's SERVER span failed without holding its handle
/// — and `http.serve` still ends the span (a `200` answer is not overridden into a 5xx; the status
/// the handler recorded is what a collector shows).
#[test]
fn handler_records_an_error_on_the_server_span() {
    let spans = emitted_spans(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         use std.{tracing}\n\
         fn fetch(req: Request): Response {\n\
         \x20   tracing.record_error(\"guardrail denied\")\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         server.serve(8080, fetch)\n",
    );
    for server in spans.iter().filter(|s| s.kind == SpanKind::Server) {
        assert_eq!(
            server.status,
            SpanStatus::Error("guardrail denied".into()),
            "the handler's error status survived to the recorded span"
        );
        assert!(server.end_unix_ms.is_some(), "http.serve still ended it");
    }
}

/// No active span: the annotations reach nothing and emit nothing — no span is invented to hold a
/// top-level annotation. (That the program can SEE this is the `false` return, pinned by
/// `tests/conformance/tracing/active_span_annotate.noe`.)
#[test]
fn top_level_annotations_emit_no_span() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         tracing.set_attribute(\"k\", 1)\n\
         tracing.add_event(\"e\")\n\
         tracing.record_error(\"boom\")\n",
    );
    assert!(
        spans.is_empty(),
        "nothing to annotate, and nothing invented: {:?}",
        names_of(&spans)
    );
}

/// Task-locality, measured on the recorder rather than only through the `bool`: a task spawned at
/// TOP LEVEL parks at a sleep while a sibling task holds a live span across its resume. The
/// orphan's annotations must land nowhere — not on the sibling's span, which a global active-span
/// stack would have handed it.
#[test]
fn a_spawned_tasks_annotations_do_not_reach_a_siblings_live_span() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         use std.task.{sleep}\n\
         async fn orphan(): bool {\n\
         \x20   sleep(3).await\n\
         \x20   return tracing.add_event(\"orphan.after\")\n\
         }\n\
         async fn holder_body(): bool {\n\
         \x20   sleep(5).await\n\
         \x20   return tracing.add_event(\"holder.own\")\n\
         }\n\
         async fn holder(): bool {\n\
         \x20   return tracing.with_span(\"holder\", holder_body).await\n\
         }\n\
         async fn race(): bool {\n\
         \x20   mut a = false\n\
         \x20   mut b = false\n\
         \x20   concurrent {\n\
         \x20       ho = spawn orphan()\n\
         \x20       hh = spawn holder()\n\
         \x20       a = ho.await\n\
         \x20       b = hh.await\n\
         \x20   }\n\
         \x20   return a || b\n\
         }\n\
         echo race().await\n",
    );

    let holder = span_named(&spans, "holder");
    // The sibling's span WAS live and reachable from its own strand — without this the test would
    // pass vacuously (nothing live to leak from).
    assert!(
        has_event(holder, "holder.own"),
        "the holder annotated its own span"
    );
    assert!(
        !has_event(holder, "orphan.after"),
        "the orphan's annotation did not leak onto the sibling's live span"
    );
    assert!(
        spans.iter().all(|s| !has_event(s, "orphan.after")),
        "the orphan had no active span at all, so its event reached nothing"
    );
}

/// The inverse leg: a task spawned INSIDE a `with_span` body inherits that span's context, so its
/// annotation reaches the spawner's span — inheritance is a snapshot, not isolation.
#[test]
fn a_task_spawned_inside_a_span_annotates_that_span() {
    let spans = emitted_spans(
        "use std.{tracing}\n\
         async fn child(): bool {\n\
         \x20   return tracing.add_event(\"child.step\")\n\
         }\n\
         async fn parent_body(): bool {\n\
         \x20   mut r = false\n\
         \x20   concurrent {\n\
         \x20       h = spawn child()\n\
         \x20       r = h.await\n\
         \x20   }\n\
         \x20   return r\n\
         }\n\
         echo tracing.with_span(\"parent\", parent_body).await\n",
    );
    let parent = span_named(&spans, "parent");
    assert!(
        has_event(parent, "child.step"),
        "the spawned task annotated the span it inherited"
    );
    assert_eq!(
        names_of(&spans),
        vec!["parent"],
        "and did not mint a child span to do it"
    );
}
