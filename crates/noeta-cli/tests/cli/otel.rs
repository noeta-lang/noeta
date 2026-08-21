//! Telemetry the way an operator configures it: environment variables on a real `noeta` process,
//! answered by a real collector on a loopback socket.
//!
//! The metrics cardinality limit is the one telemetry knob whose failure mode is a *leak* rather
//! than a missing signal, so it is proved here on the whole path — `OTEL_METRIC_CARDINALITY_LIMIT`
//! → `RealHost`'s metric store → the periodic reader's final export → the OTLP/JSON body a
//! collector actually receives. The store's own semantics are unit-tested in `noeta-ext-abi` and
//! held to both backends by `noeta-conformance`'s metrics oracle; what only a subprocess can show is
//! that the variable is read at all.

use std::io::{Read, Write};
use std::net::TcpListener;

use super::support::*;

/// A stand-in OTLP collector: accepts one POST, reads it whole (headers plus `content-length`
/// bytes), answers `200`, and hands the body back. Returned with the base endpoint to configure the
/// program with.
fn stub_collector() -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("the exporter connects");
        sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .ok();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            // Read until the headers are complete and `content-length` bytes have followed them.
            let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
            if let Some(head_end) = head_end {
                let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
                let len: usize = head
                    .split("content-length:")
                    .nth(1)
                    .and_then(|rest| rest.split(['\r', '\n']).next())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if raw.len() >= head_end + len {
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
                    let _ = sock.flush();
                    return String::from_utf8_lossy(&raw[head_end..head_end + len]).into_owned();
                }
            }
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    });
    (endpoint, handle)
}

/// Every data point of the one exported sum metric, as `(attributes, value)`.
fn sum_points(body: &str) -> Vec<(serde_json::Value, i64)> {
    let json: serde_json::Value = serde_json::from_str(body).expect("the collector received JSON");
    json["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
        .as_array()
        .expect("the body carries a sum's data points")
        .iter()
        .map(|p| {
            let value = p["asInt"]
                .as_str()
                .expect("an integer counter exports `asInt`")
                .parse()
                .expect("a number");
            (p["attributes"].clone(), value)
        })
        .collect()
}

/// A counter carrying a distinct attribute value per measurement — the mistake the cardinality
/// limit exists for. `OTEL_METRIC_CARDINALITY_LIMIT` bounds what the collector receives, and the
/// measurements past the bound arrive folded into the spec's `otel.metric.overflow` series rather
/// than dropped, so the counter's total is still exactly the number of measurements.
#[test]
fn the_cardinality_limit_bounds_what_a_collector_receives() {
    let program = temp_program(
        "otel_cardinality",
        "use std.{metrics}\n\
         c = metrics.counter(\"http.requests\")\n\
         mut i = 0\n\
         while i < 200 {\n\
             c.add_with(1, {\"request.id\": i})\n\
             i = i + 1\n\
         }\n\
         echo \"done\"\n",
    );
    let (endpoint, collector) = stub_collector();

    lang()
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env("OTEL_SERVICE_NAME", "cardinality-test")
        .env("OTEL_METRIC_CARDINALITY_LIMIT", "8")
        .arg("run")
        .arg(&program)
        .assert()
        .success()
        .stdout(predicate::str::contains("done"));

    let body = collector.join().expect("the collector thread finishes");
    let points = sum_points(&body);
    assert_eq!(
        points.len(),
        9,
        "8 ordinary series plus the one overflow series, not 200; body was:\n{body}"
    );

    let overflow_attr = serde_json::json!([{
        "key": "otel.metric.overflow",
        "value": { "boolValue": true },
    }]);
    let overflow: Vec<i64> = points
        .iter()
        .filter(|(attrs, _)| *attrs == overflow_attr)
        .map(|(_, v)| *v)
        .collect();
    assert_eq!(
        overflow,
        vec![192],
        "the 192 sets past the limit fold into one marked series; body was:\n{body}"
    );
    assert_eq!(
        points.iter().map(|(_, v)| *v).sum::<i64>(),
        200,
        "folding, not dropping — the counter's total is every measurement"
    );
}

/// Unset, the limit is the OTel default of 2000 — far above this program's cardinality, so every
/// attribute set keeps its own series and nothing is marked as overflow. The counterpart to the test
/// above: it is the *variable* that changes the answer, not the presence of the cap.
#[test]
fn without_the_variable_the_default_limit_leaves_a_small_program_untouched() {
    let program = temp_program(
        "otel_cardinality_default",
        "use std.{metrics}\n\
         c = metrics.counter(\"http.requests\")\n\
         mut i = 0\n\
         while i < 200 {\n\
             c.add_with(1, {\"request.id\": i})\n\
             i = i + 1\n\
         }\n\
         echo \"done\"\n",
    );
    let (endpoint, collector) = stub_collector();

    lang()
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .env("OTEL_SERVICE_NAME", "cardinality-test")
        .env_remove("OTEL_METRIC_CARDINALITY_LIMIT")
        .arg("run")
        .arg(&program)
        .assert()
        .success()
        .stdout(predicate::str::contains("done"));

    let body = collector.join().expect("the collector thread finishes");
    let points = sum_points(&body);
    assert_eq!(
        points.len(),
        200,
        "200 distinct sets, all under the default"
    );
    assert!(
        !body.contains("otel.metric.overflow"),
        "nothing overflowed; body was:\n{body}"
    );
}

/// A limit an operator mistyped must not silently blind every instrument in the program. A
/// malformed value falls back to the default, so this program exports its 200 series exactly as it
/// would with the variable unset — the failure mode of reading `"none"` as `0` would be a single
/// overflow point and no breakdown at all.
#[test]
fn a_malformed_limit_falls_back_to_the_default() {
    let program = temp_program(
        "otel_cardinality_malformed",
        "use std.{metrics}\n\
         c = metrics.counter(\"http.requests\")\n\
         mut i = 0\n\
         while i < 200 {\n\
             c.add_with(1, {\"request.id\": i})\n\
             i = i + 1\n\
         }\n\
         echo \"done\"\n",
    );

    for raw in ["none", "0"] {
        let (endpoint, collector) = stub_collector();
        lang()
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
            .env("OTEL_SERVICE_NAME", "cardinality-test")
            .env("OTEL_METRIC_CARDINALITY_LIMIT", raw)
            .arg("run")
            .arg(&program)
            .assert()
            .success()
            .stdout(predicate::str::contains("done"));

        let body = collector.join().expect("the collector thread finishes");
        assert_eq!(
            sum_points(&body).len(),
            200,
            "{raw:?} is not a usable limit, so the default 2000 stands; body was:\n{body}"
        );
    }
}
