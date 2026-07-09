//! Native OTEL Phase L — the `std.log` logs signal, sink-parity oracle.
//!
//! A log record is a *write-only side effect* — invisible to program output, so the differential
//! can't see it. This observes the emitted [`LogRecord`]s directly: it runs a program on a
//! [`SandboxHost`] whose recorder feeds a shared sink ([`SandboxHost::set_log_sink`]) that outlives
//! the host (the VM consumes it), then asserts on the records — and runs the SAME program on the
//! tree-walker with its own sink, asserting the two backends record **byte-identical** logs
//! (correlation ids included). The logs twin of the `tracing_serve` span oracle.

use std::sync::{Arc, Mutex};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::{AttrValue, LogRecord, SandboxHost, Severity};
use noeta_vm::VmBackend;

fn compile(text: &str) -> noeta_bytecode::Module {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "logs.noe", text);
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

/// Run `text` on both backends with a log sink installed and assert they emit identical records;
/// return them (in emission order).
fn emitted_logs(text: &str) -> Vec<LogRecord> {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "logs.noe", text);
    let src = noeta_db::source_program(&db, &source);
    let module = compile(text);

    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut host = SandboxHost::new();
    host.set_log_sink(sink.clone());
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    let vm_logs: Vec<LogRecord> = sink.lock().unwrap().clone();

    let program = noeta_db::ast(&db, src).0.program.clone();
    let sites = noeta_db::checked(&db, src).sites.clone();
    let tree_sink = Arc::new(Mutex::new(Vec::new()));
    let mut tree_host = SandboxHost::new();
    tree_host.set_log_sink(tree_sink.clone());
    let tree_result = noeta_conformance::reference::reference_run_with_host(
        &program,
        sites,
        Box::new(tree_host),
    );
    assert_eq!(
        result.is_ok(),
        tree_result.is_ok(),
        "both backends agree on the exit disposition"
    );
    let tree_logs: Vec<LogRecord> = tree_sink.lock().unwrap().clone();
    assert_eq!(
        vm_logs, tree_logs,
        "both backends record identical logs (telemetry parity)"
    );
    assert!(result.is_ok(), "program ran cleanly");
    vm_logs
}

/// A top-level log has no active span, so it carries no trace correlation.
#[test]
fn top_level_log_has_no_trace_correlation() {
    let logs = emitted_logs(
        "use std.{log}\n\
         log.info(\"starting up\")\n",
    );
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].body, "starting up");
    assert_eq!(logs[0].severity, Severity::Info);
    assert!(logs[0].trace_context.is_none(), "no active span → no correlation");
    assert!(logs[0].attributes.is_empty());
}

/// The convenience levels map to their severities.
#[test]
fn convenience_levels_carry_their_severity() {
    let logs = emitted_logs(
        "use std.{log}\n\
         log.debug(\"d\")\n\
         log.info(\"i\")\n\
         log.warn(\"w\")\n\
         log.error(\"e\")\n",
    );
    let got: Vec<(&str, Severity)> = logs
        .iter()
        .map(|r| (r.body.as_str(), r.severity))
        .collect();
    assert_eq!(
        got,
        [
            ("d", Severity::Debug),
            ("i", Severity::Info),
            ("w", Severity::Warn),
            ("e", Severity::Error),
        ]
    );
}

/// The generic `log(severity, message)` parses the severity string (case-insensitive), reaching the
/// levels the conveniences don't name (`trace`/`fatal`); an unknown severity falls back to `info`.
#[test]
fn generic_log_parses_severity_with_info_fallback() {
    let logs = emitted_logs(
        "use std.{log}\n\
         log.log(\"TRACE\", \"t\")\n\
         log.log(\"fatal\", \"f\")\n\
         log.log(\"bogus\", \"b\")\n",
    );
    let got: Vec<(&str, Severity)> = logs
        .iter()
        .map(|r| (r.body.as_str(), r.severity))
        .collect();
    assert_eq!(
        got,
        [
            ("t", Severity::Trace),
            ("f", Severity::Fatal),
            ("b", Severity::Info),
        ]
    );
}

/// The headline feature: a log emitted inside a `with_span` auto-correlates to that span — it
/// carries the span's trace id and its span id — with zero threading from the user.
#[test]
fn log_inside_a_span_correlates_to_it() {
    let logs = emitted_logs(
        "use std.{log}\n\
         use std.{tracing}\n\
         body = fn(): int {\n\
         \x20   log.warn(\"inside\")\n\
         \x20   return 1\n\
         }\n\
         tracing.with_span(\"job\", body)\n\
         log.info(\"outside\")\n",
    );
    assert_eq!(logs.len(), 2);
    // The record inside the span carries a correlation context…
    let inside = &logs[0];
    assert_eq!(inside.body, "inside");
    let ctx = inside.trace_context.expect("inside-span log is correlated");
    // …a non-zero trace id and span id (the active span's).
    assert_ne!(ctx.trace_id, [0u8; 16]);
    assert_ne!(ctx.span_id, [0u8; 8]);
    // The record after the span closed has none.
    assert_eq!(logs[1].body, "outside");
    assert!(logs[1].trace_context.is_none(), "span ended → no correlation");
}

/// L2 — the `*_with` forms carry a `Map<string, string|int|float|bool>` of structured attributes
/// onto the record (a heterogeneous map literal absorbing the union value type at the call site),
/// byte-identical across backends. Map iteration order is deterministic, so the recorded attribute
/// order agrees.
#[test]
fn structured_attributes_ride_the_record() {
    let logs = emitted_logs(
        "use std.{log}\n\
         log.info_with(\"served\", {\"route\": \"/users\", \"status\": 200, \"cached\": true, \"ms\": 1.5})\n",
    );
    assert_eq!(logs.len(), 1);
    let r = &logs[0];
    assert_eq!(r.body, "served");
    assert_eq!(r.severity, Severity::Info);
    // Each scalar value projects to its `AttrValue` variant.
    let get = |k: &str| r.attributes.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    assert_eq!(get("route"), Some(&AttrValue::Str("/users".into())));
    assert_eq!(get("status"), Some(&AttrValue::Int(200)));
    assert_eq!(get("cached"), Some(&AttrValue::Bool(true)));
    assert_eq!(get("ms"), Some(&AttrValue::Float(1.5)));
    assert_eq!(r.attributes.len(), 4);
}

/// L2 — the generic `log_with(severity, message, attrs)` form and a `*_with` inside a span (the
/// attributes and the trace correlation combine).
#[test]
fn attributed_log_inside_a_span_correlates_and_carries_attrs() {
    let logs = emitted_logs(
        "use std.{log}\n\
         use std.{tracing}\n\
         body = fn(): int {\n\
         \x20   log.error_with(\"failed\", {\"code\": 500})\n\
         \x20   return 1\n\
         }\n\
         tracing.with_span(\"job\", body)\n",
    );
    assert_eq!(logs.len(), 1);
    let r = &logs[0];
    assert_eq!(r.severity, Severity::Error);
    assert_eq!(
        r.attributes.iter().find(|(n, _)| n == "code").map(|(_, v)| v),
        Some(&AttrValue::Int(500))
    );
    assert!(r.trace_context.is_some(), "attributed log still correlates");
}

/// Nested `with_span`s correlate a log to the *innermost* active span (the top of the active-span
/// stack): the outer-scope log carries the outer span's id, the inner-scope log the inner span's —
/// same trace, different span.
#[test]
fn log_correlates_to_the_innermost_span() {
    let logs = emitted_logs(
        "use std.{log}\n\
         use std.{tracing}\n\
         inner = fn(): int {\n\
         \x20   log.info(\"inner-scope\")\n\
         \x20   return 1\n\
         }\n\
         outer = fn(): int {\n\
         \x20   log.info(\"outer-scope\")\n\
         \x20   tracing.with_span(\"inner\", inner)\n\
         \x20   return 1\n\
         }\n\
         tracing.with_span(\"outer\", outer)\n",
    );
    assert_eq!(logs.len(), 2);
    let outer_ctx = logs[0].trace_context.expect("outer-scope log correlated");
    let inner_ctx = logs[1].trace_context.expect("inner-scope log correlated");
    assert_eq!(logs[0].body, "outer-scope");
    assert_eq!(logs[1].body, "inner-scope");
    assert_eq!(
        outer_ctx.trace_id, inner_ctx.trace_id,
        "nested spans share one trace"
    );
    assert_ne!(
        outer_ctx.span_id, inner_ctx.span_id,
        "each log correlates to its own (innermost) active span"
    );
}
