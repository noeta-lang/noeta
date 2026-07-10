//! Native OTEL Phase M — the `std.metrics` metrics signal, sink-parity oracle.
//!
//! A metric is a *write-only side effect* — invisible to program output. This runs a program on a
//! [`SandboxHost`] whose aggregation feeds a shared sink ([`SandboxHost::set_metric_sink`]) that
//! collects **at teardown** (the sandbox's deterministic collection point — the host is dropped at
//! end-of-run), then asserts on the collected [`MetricData`] — and runs the SAME program on the
//! tree-walker with its own sink, asserting the two backends aggregate **byte-identically** (sums,
//! histogram buckets, per-attribute-set series, sorted order). The metrics twin of the span/log
//! oracles.

use std::sync::{Arc, Mutex};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::{MetricData, MetricPoints, MetricValue, SandboxHost};
use noeta_vm::VmBackend;

fn compile(text: &str) -> noeta_bytecode::Module {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "metrics.noe", text);
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

/// Run `text` on both backends with a metric sink installed and assert they collect identical
/// metrics at teardown; return them (in instrument-creation order).
fn collected_metrics(text: &str) -> Vec<MetricData> {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "metrics.noe", text);
    let src = noeta_db::source_program(&db, &source);
    let module = compile(text);

    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut host = SandboxHost::new();
    host.set_metric_sink(sink.clone());
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    let vm_metrics: Vec<MetricData> = sink.lock().unwrap().clone();

    let program = noeta_db::ast(&db, src).0.program.clone();
    let sites = noeta_db::checked(&db, src).sites.clone();
    let tree_sink = Arc::new(Mutex::new(Vec::new()));
    let mut tree_host = SandboxHost::new();
    tree_host.set_metric_sink(tree_sink.clone());
    let tree_result =
        noeta_conformance::reference::reference_run_with_host(&program, sites, Box::new(tree_host));
    assert_eq!(
        result.is_ok(),
        tree_result.is_ok(),
        "both backends agree on the exit disposition"
    );
    let tree_metrics: Vec<MetricData> = tree_sink.lock().unwrap().clone();
    assert_eq!(
        vm_metrics, tree_metrics,
        "both backends collect identical metrics (telemetry parity)"
    );
    assert!(result.is_ok(), "program ran cleanly");
    vm_metrics
}

fn number_points(m: &MetricData) -> &[noeta_stdlib::NumberPoint] {
    match &m.points {
        MetricPoints::Sum { points, .. } | MetricPoints::Gauge(points) => points,
        MetricPoints::Histogram(_) => panic!("expected a number metric"),
    }
}

/// A Counter aggregates a monotonic sum, one series per attribute set, sorted by attribute key.
#[test]
fn counter_sums_per_attribute_set() {
    let metrics = collected_metrics(
        "use std.{metrics}\n\
         c = metrics.counter(\"http.requests\")\n\
         c.add(1)\n\
         c.add_with(1, {\"method\": \"GET\"})\n\
         c.add_with(1, {\"method\": \"POST\"})\n\
         c.add_with(1, {\"method\": \"GET\"})\n",
    );
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].name, "http.requests");
    let MetricPoints::Sum { points, monotonic } = &metrics[0].points else {
        panic!("counter is a sum");
    };
    assert!(monotonic, "a counter is monotonic");
    // Three series: the empty set, GET, POST — sorted by attribute-set key (empty first).
    assert_eq!(points.len(), 3);
    assert_eq!(points[0].value, MetricValue::Int(1)); // no attrs
    assert!(points[0].attributes.is_empty());
    assert_eq!(points[1].value, MetricValue::Int(2)); // GET ×2
    assert_eq!(points[2].value, MetricValue::Int(1)); // POST
}

/// An UpDownCounter is a non-monotonic sum; a Gauge keeps the last value; both collect one series.
#[test]
fn updown_and_gauge_semantics() {
    let metrics = collected_metrics(
        "use std.{metrics}\n\
         active = metrics.up_down_counter(\"active\")\n\
         active.add(3)\n\
         active.add(-1)\n\
         g = metrics.gauge(\"temp\")\n\
         g.record(20.0)\n\
         g.record(22.5)\n",
    );
    let active = metrics.iter().find(|m| m.name == "active").unwrap();
    let MetricPoints::Sum { points, monotonic } = &active.points else {
        panic!("updown is a sum");
    };
    assert!(!monotonic);
    assert_eq!(points[0].value, MetricValue::Int(2)); // 3 - 1

    let temp = metrics.iter().find(|m| m.name == "temp").unwrap();
    assert_eq!(number_points(temp)[0].value, MetricValue::Float(22.5)); // last wins
}

/// A Histogram buckets observations over the OTel default bounds and tracks count + sum.
#[test]
fn histogram_buckets_observations() {
    let metrics = collected_metrics(
        "use std.{metrics}\n\
         h = metrics.histogram(\"latency\")\n\
         h.record(3)\n\
         h.record(7.0)\n\
         h.record(600)\n",
    );
    let MetricPoints::Histogram(points) = &metrics[0].points else {
        panic!("latency is a histogram");
    };
    assert_eq!(points[0].count, 3);
    assert_eq!(points[0].sum, 610.0);
    assert_eq!(points[0].buckets[1], 1); // 3 → <=5
    assert_eq!(points[0].buckets[2], 1); // 7 → <=10
    assert_eq!(points[0].buckets[9], 1); // 600 → <=750
    assert_eq!(points[0].buckets.iter().sum::<u64>(), 3);
}

/// M3 — server auto-instrumentation: every accepted request records the
/// `http.server.request.duration` histogram (one series per method/route/status) and balances
/// `http.server.active_requests` (net zero after all requests complete). The sandbox drives its
/// fixed five-request script, so the collected metrics are deterministic and byte-identical across
/// backends. The metrics twin of the SERVER-span auto-instrumentation.
#[test]
fn server_serve_auto_instruments_request_metrics() {
    let metrics = collected_metrics(
        "use std.http.server\n\
         use std.http.{Request, Response}\n\
         fn fetch(req: Request): Response { return server.response(200, \"ok\") }\n\
         server.serve(8080, fetch)\n",
    );

    let duration = metrics
        .iter()
        .find(|m| m.name == "http.server.request.duration")
        .expect("duration histogram recorded");
    let MetricPoints::Histogram(points) = &duration.points else {
        panic!("request duration is a histogram");
    };
    // The scripted requests: GET /, GET /health, POST /echo, GET /users/42, DELETE /users/42 — five
    // distinct (method, route, status) series, each observed once.
    assert_eq!(points.len(), 5, "one series per distinct request");
    assert!(
        points.iter().all(|p| p.count == 1),
        "each request observed once"
    );

    let active = metrics
        .iter()
        .find(|m| m.name == "http.server.active_requests")
        .expect("active-requests counter recorded");
    let MetricPoints::Sum { points, monotonic } = &active.points else {
        panic!("active_requests is a sum");
    };
    assert!(!monotonic, "active_requests is an up/down counter");
    // Every series balances to zero once its request completes (a +1 and a −1).
    assert!(
        points.iter().all(|p| p.value == MetricValue::Int(0)),
        "active_requests returns to zero"
    );
}

/// Get-or-create is idempotent by name: two `counter("x")` calls share one host-side instrument, so
/// their adds accumulate into a single series.
#[test]
fn get_or_create_is_idempotent_by_name() {
    let metrics = collected_metrics(
        "use std.{metrics}\n\
         a = metrics.counter(\"hits\")\n\
         b = metrics.counter(\"hits\")\n\
         a.add(1)\n\
         b.add(1)\n",
    );
    assert_eq!(metrics.len(), 1, "one instrument, not two");
    assert_eq!(number_points(&metrics[0])[0].value, MetricValue::Int(2));
}
