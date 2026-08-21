//! The `Network` capability's deterministic sandbox responder (http arc H1) — the [`crate::fs::Vfs`]
//! analog for the network. The seam *types* (`NetRequest`/`NetResponse`/`NetFetchIo`) live in the
//! ABI crate ([`noeta_ext_abi::net`], re-exported here); the responder stays here because it uses
//! `serde_json`, which the lean ABI crate deliberately does not pull.
//!
//! A network has no program-visible "write" step, so unlike the Vfs there is no mutable store to
//! seed: the sandbox responder is a **pure function of the request**, deterministic by
//! construction, honoring a small httpbin-style control grammar so conformance can exercise every
//! response path and pin exact bytes. Under the sandbox every request — whatever its URL — is
//! answered here; a program that wants real data runs under `noeta run` (the real host).

pub use noeta_ext_abi::net::{
    HTTP_ERROR_TYPE_IDENTITY, HTTP_ERROR_TYPE_NAME, NetError, NetErrorKind, NetFetchIo, NetRequest,
    NetResponse, REQUEST_TYPE_IDENTITY, REQUEST_TYPE_NAME, RESPONSE_TYPE_IDENTITY,
    RESPONSE_TYPE_NAME, Request, WS_ACCEPT_GUID, accept_outcome, fetch_outcome, form_pairs,
    form_value, percent_decode, percent_encode, query_value, request_header, request_path,
    ws_recv_outcome,
};

use serde_json::json;

/// The sandbox's fixed **inbound** request script (http-server S1) — the deterministic driver a
/// served program's handler runs against under `--differential`, the inbound mirror of the pure
/// `sandbox_respond`. A finite, documented sequence, so conformance can pin a handler's behavior
/// and the served program terminates in-oracle (a real accept loop never would). Every
/// `http.serve` under the sandbox is driven by this exact sequence, whatever port it names:
///
///   1. `GET /`                                — root, no body, no headers
///   2. `GET /health`                          — a second path to route on
///   3. `POST /echo`  body `hello`  header `content-type: text/plain`  — a body + a header
///   4. `GET /users/42?active=true`            — a path segment + a query string
///   5. `DELETE /users/42`                     — a non-GET/POST verb
///   6. `POST /form`  body `title=buy+milk&note=caf%C3%A9`  header
///      `content-type: application/x-www-form-urlencoded` — a form submission whose fields need
///      percent-decoding, including a multi-byte character (`req.form(name)`/`form_all()`)
///   7. `GET /ws`  headers `upgrade: websocket`, `sec-websocket-key: <fixed>`  — a websocket
///      upgrade request (server-hmr L0). A handler that upgrades it is driven by the fixed
///      client conversation ([`sandbox_ws_client_frames`]); one that responds normally treats
///      it as any other GET.
///   8. `GET /events`  header `accept: text/event-stream`  — a request a handler may answer with
///      `server.sse` (http-streaming arc). It carries no upgrade headers because SSE needs none:
///      any request can be answered with an event stream, so a handler that ignores it serves it
///      as an ordinary GET, exactly like `/ws`.
///
/// **Adding an entry here is not free.** Several conformance cases pin one output line per
/// scripted request, and the Rust tests that count them derive their expected count from
/// `sandbox_request_script().len()` — keep it that way rather than hardcoding a number, so the
/// next entry costs a corpus update and not a hunt for stale integers.
pub fn sandbox_request_script() -> Vec<NetRequest> {
    let req = |method: &str, path: &str, body: &str, headers: Vec<(&str, &str)>| NetRequest {
        method: method.to_string(),
        url: path.to_string(),
        headers: headers
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
        timeout_ms: None,
        redirect_limit: None,
    };
    vec![
        req("GET", "/", "", vec![]),
        req("GET", "/health", "", vec![]),
        req(
            "POST",
            "/echo",
            "hello",
            vec![("content-type", "text/plain")],
        ),
        req("GET", "/users/42?active=true", "", vec![]),
        req("DELETE", "/users/42", "", vec![]),
        req(
            "POST",
            "/form",
            "title=buy+milk&note=caf%C3%A9",
            vec![("content-type", "application/x-www-form-urlencoded")],
        ),
        req(
            "GET",
            "/ws",
            "",
            vec![
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                // A fixed, valid 16-byte base64 key so the accept-key derivation is exercised
                // deterministically end to end.
                ("sec-websocket-key", "c2FuZGJveC13cy1rZXkhIQ=="),
            ],
        ),
        req("GET", "/events", "", vec![("accept", "text/event-stream")]),
    ]
}

/// The sandbox's fixed **websocket client conversation** (server-hmr L0) — the frames "the peer"
/// sends on any upgraded connection, then a clean close (recv yields `None`). The ws analog of
/// the request script: finite and documented, so an upgraded handler's behavior pins exactly and
/// the serve loop terminates in-oracle.
pub fn sandbox_ws_client_frames() -> Vec<String> {
    vec!["first frame".to_string(), "second frame".to_string()]
}

/// The sandbox's deterministic **streaming** response body (http-streaming arc) — the incremental
/// twin of [`sandbox_respond`], and a pure function of the request for the same reason: both
/// backends must compute the identical byte sequence or the differential cannot hold.
///
/// A real streaming host hands over bytes as the network produces them. The sandbox has no
/// network, so it produces the whole body up front and the stream doles it out; what matters for
/// the oracle is that the *frames* are identical, not that the chunking is realistic. Chunk-split
/// behavior is covered exhaustively by the decoder's own byte-by-byte unit tests, where it can be
/// asserted directly instead of inferred from a program's output.
///
/// Control grammar (by request path), chosen so conformance can pin every framing and every
/// interesting body shape:
/// - `/stream/sse` → an event stream exercising the corners a provider actually leans on: a named
///   event, a multi-line `data:`, an `id:` that persists, a `retry:`, a `: keepalive` comment that
///   dispatches nothing, and a terminal `[DONE]`.
/// - `/stream/ndjson` → three JSON documents, one per line.
/// - `/stream/lines` → three lines with a blank one in the middle (which `Lines` keeps and
///   `Ndjson` would drop).
/// - `/stream/empty` → a zero-byte body: the stream ends immediately, `recv` yields `none` first
///   time.
/// - `/stream/truncated` → an SSE body cut off mid-block, with no terminating blank line: the
///   complete frame arrives and the partial one is discarded.
/// - `/stream/error` → what a rate-limited vendor actually answers: a bare JSON error document,
///   served with a non-2xx head ([`sandbox_stream_head`]). It is **not** an event stream, so under
///   [`noeta_ext_abi::stream::Framing::Sse`] it decodes to zero frames — which is precisely the
///   failure `FrameStream.status()` exists to make visible, and why the body is scripted here
///   rather than described in prose.
/// - anything else → a single-frame body, so a stream against any URL still terminates.
pub fn sandbox_stream_body(request: &NetRequest) -> String {
    match noeta_ext_abi::uri::path_of(&request.url) {
        "/stream/sse" => concat!(
            "event: token\ndata: He\n\n",
            "data: multi\ndata: line\n\n",
            ": keepalive\n\n",
            "id: 7\nretry: 1500\ndata: tagged\n\n",
            "data: [DONE]\n\n",
        )
        .to_string(),
        "/stream/ndjson" => "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n".to_string(),
        "/stream/lines" => "alpha\n\nbeta\n".to_string(),
        "/stream/empty" => String::new(),
        // No terminating blank line after `partial` — the truncated-body case.
        "/stream/truncated" => "data: complete\n\ndata: partial".to_string(),
        // A vendor's rate-limit document, verbatim in the shape one arrives in: one line of JSON,
        // no `data:` prefix, no blank line. Deliberately NOT event-stream syntax.
        "/stream/error" => {
            "{\"error\":{\"message\":\"rate limit exceeded\",\"type\":\"rate_limit_error\"}}"
                .to_string()
        }
        path => format!("data: noeta sandbox: {} {path}\n\n", request.method),
    }
}

/// The **head** of the sandbox's scripted streaming response: the status and headers that come back
/// with the opening handshake, before a single body byte is decoded.
///
/// The status grammar is [`sandbox_respond`]'s, not a second one: every path takes the status the
/// buffered responder would have given it, so `/status/503` means the same thing streamed as it does
/// buffered and there is one table to reason about. `/stream/error` is the single exception, and it
/// exists to script the case the buffered grammar cannot express — a non-2xx whose body is a JSON
/// document rather than an event stream, carrying the `retry-after` a backoff actually needs.
pub fn sandbox_stream_head(request: &NetRequest) -> (u16, Vec<(String, String)>) {
    let header = |name: &str, value: &str| (name.to_string(), value.to_string());
    match noeta_ext_abi::uri::path_of(&request.url) {
        "/stream/error" => (
            429,
            vec![
                header("content-type", "application/json"),
                header("retry-after", "30"),
            ],
        ),
        _ => (
            sandbox_respond(request).status,
            vec![header(
                "content-type",
                noeta_ext_abi::stream::SSE_CONTENT_TYPE,
            )],
        ),
    }
}

/// Decode the sandbox's scripted body for `request` into the frames a `FrameStream` will hand out.
///
/// Shared by the sandbox host so the *same* [`noeta_ext_abi::stream::FrameDecoder`] the real host
/// uses does the cutting — the framing semantics are proven once and cannot diverge between the
/// oracle and production.
pub fn sandbox_stream_frames(
    request: &NetRequest,
    framing: noeta_ext_abi::stream::Framing,
) -> Vec<noeta_ext_abi::stream::Frame> {
    let mut decoder = noeta_ext_abi::stream::FrameDecoder::new(framing);
    decoder.feed_str(&sandbox_stream_body(request));
    decoder.finish();
    std::iter::from_fn(|| decoder.next_frame()).collect()
}

/// A response with a single `content-type` header (the responder's shorthand).
fn typed(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> NetResponse {
    NetResponse {
        status,
        headers: vec![("content-type".to_string(), content_type.to_string())],
        body: body.into(),
        // Stamped by `sandbox_respond`, which is the only caller and the only place that knows the
        // request — keeping it off this shorthand avoids threading the URL through every arm.
        url: String::new(),
    }
}

/// The deterministic sandbox response for `request` — the whole `Network` capability on
/// [`crate::SandboxHost`]. Pure, so both backends compute the identical response and the
/// differential holds.
///
/// Control grammar (by request path):
/// - `/status/{n}` → an empty response with status `n` (a malformed or out-of-range `n` is `400`).
/// - `/echo` → `200`, JSON body `{method, path, body}` echoing the request.
/// - `/headers` → `200`, JSON body of the request headers (sorted by name).
/// - `/redirect/{n}` → a `302` to `/redirect/{n-1}`, so `/redirect/3` is a three-hop chain;
///   `/redirect/0` is the destination and answers `200 arrived`. A relative `Location`, which is
///   what makes it exercise resolution against the hop it came from rather than the original URL.
/// - `/redirect-status/{n}` → status `n` with `Location: /echo`, so a program can watch what each
///   redirecting status does to its method and body — `/echo` reports both back.
/// - `/redirect-same` → a `302` to a relative `/headers`, and `/redirect-cross` → a `302` to an
///   **absolute** `Location` on a different host (`https://other.test/headers`). The pair exists
///   because "credentials do not cross an origin" is only half a rule without "and they do survive
///   a hop that stays put" — `/headers` answers with whatever reached it, so a program can see
///   both halves.
/// - anything else → `200`, the plain-text line `noeta sandbox: {method} {path}`.
pub fn sandbox_respond(request: &NetRequest) -> NetResponse {
    // Every arm builds its response through `typed`, which cannot know the request; stamp the
    // originating URL once here so a sandbox response carries it exactly as a real one does.
    let mut response = sandbox_body(request);
    response.url = request.url.clone();
    response
}

/// A redirecting response: `status` plus the `Location` that names where to go.
fn redirect_to(status: u16, location: &str) -> NetResponse {
    NetResponse {
        status,
        headers: vec![("location".to_string(), location.to_string())],
        body: Vec::new(),
        url: String::new(),
    }
}

/// The host a `/redirect-cross` hop lands on — a second origin, so the sandbox can script the one
/// redirect case that is a security rule rather than a convenience.
pub const SANDBOX_CROSS_ORIGIN: &str = "https://other.test";

fn sandbox_body(request: &NetRequest) -> NetResponse {
    let path = noeta_ext_abi::uri::path_of(&request.url);
    if let Some(rest) = path.strip_prefix("/status/") {
        return match rest.parse::<u16>() {
            Ok(n) if (100..=599).contains(&n) => typed(n, "text/plain", ""),
            _ => typed(400, "text/plain", "invalid status"),
        };
    }
    if let Some(rest) = path.strip_prefix("/redirect/") {
        return match rest.parse::<u32>() {
            Ok(0) => typed(200, "text/plain", "arrived"),
            Ok(n) => redirect_to(302, &format!("/redirect/{}", n - 1)),
            Err(_) => typed(400, "text/plain", "invalid hop count"),
        };
    }
    if let Some(rest) = path.strip_prefix("/redirect-status/") {
        return match rest.parse::<u16>() {
            Ok(n) if (300..=399).contains(&n) => redirect_to(n, "/echo"),
            _ => typed(400, "text/plain", "invalid redirect status"),
        };
    }
    match path {
        "/redirect-same" => redirect_to(302, "/headers"),
        "/redirect-cross" => redirect_to(302, &format!("{SANDBOX_CROSS_ORIGIN}/headers")),
        "/echo" => {
            let doc = json!({
                "method": request.method,
                "path": path,
                "body": String::from_utf8_lossy(&request.body),
            });
            typed(200, "application/json", doc.to_string())
        }
        "/headers" => {
            // BTreeMap → JSON object with keys sorted, so the body is stable regardless of header
            // insertion order.
            let sorted: std::collections::BTreeMap<&str, &str> = request
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            typed(200, "application/json", json!(sorted).to_string())
        }
        _ => typed(
            200,
            "text/plain",
            format!("noeta sandbox: {} {path}", request.method),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str) -> NetRequest {
        NetRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: vec![],
            body: vec![],
            timeout_ms: None,
            redirect_limit: None,
        }
    }

    #[test]
    fn the_redirect_route_is_a_chain_that_ends() {
        // `/redirect/n` counts down to `/redirect/0`, which is the destination.
        let hop = sandbox_respond(&get("https://x.test/redirect/3"));
        assert_eq!(hop.status, 302);
        assert_eq!(
            hop.header_value("location"),
            Some("/redirect/2"),
            "relative on purpose: following it exercises resolution against the current hop"
        );
        let end = sandbox_respond(&get("https://x.test/redirect/0"));
        assert_eq!(end.status, 200);
        assert_eq!(String::from_utf8(end.body).unwrap(), "arrived");
        assert_eq!(
            sandbox_respond(&get("https://x.test/redirect/zzz")).status,
            400
        );
    }

    #[test]
    fn the_redirect_status_route_answers_with_the_status_it_is_asked_for() {
        for status in [301, 302, 303, 307, 308] {
            let response =
                sandbox_respond(&get(&format!("https://x.test/redirect-status/{status}")));
            assert_eq!(response.status, status);
            assert_eq!(
                response.header_value("location"),
                Some("/echo"),
                "it lands on the route that reports the method and body that survived"
            );
        }
        // Only a 3xx makes sense here; anything else is a malformed request for the route.
        assert_eq!(
            sandbox_respond(&get("https://x.test/redirect-status/200")).status,
            400
        );
    }

    #[test]
    fn the_credential_pair_differs_only_in_where_it_points() {
        // Same origin, relative target…
        let same = sandbox_respond(&get("https://x.test/redirect-same"));
        assert_eq!(same.header_value("location"), Some("/headers"));
        // …and the same destination on a second origin, spelled absolutely. The pair is what makes
        // "credentials cross a hop that stays put, and never one that does not" observable from a
        // program: both land on `/headers`, which reports what arrived.
        let cross = sandbox_respond(&get("https://x.test/redirect-cross"));
        assert_eq!(
            cross.header_value("location"),
            Some("https://other.test/headers")
        );
        assert!(!noeta_ext_abi::uri::same_origin(
            "https://x.test/redirect-cross",
            cross.header_value("location").unwrap()
        ));
    }

    #[test]
    fn status_route_returns_the_named_status() {
        assert_eq!(
            sandbox_respond(&get("https://x.test/status/404")).status,
            404
        );
        assert_eq!(
            sandbox_respond(&get("https://x.test/status/204")).status,
            204
        );
        // Malformed or out-of-range → 400.
        assert_eq!(
            sandbox_respond(&get("https://x.test/status/zzz")).status,
            400
        );
        assert_eq!(
            sandbox_respond(&get("https://x.test/status/9999")).status,
            400
        );
    }

    #[test]
    fn echo_is_deterministic_json_of_the_request() {
        let mut req = get("https://x.test/echo");
        req.method = "POST".to_string();
        req.body = b"hi".to_vec();
        let resp = sandbox_respond(&req);
        assert_eq!(resp.status, 200);
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            r#"{"body":"hi","method":"POST","path":"/echo"}"#
        );
    }

    #[test]
    fn headers_route_sorts_by_name() {
        let mut req = get("https://x.test/headers");
        req.headers = vec![
            ("x-b".to_string(), "2".to_string()),
            ("x-a".to_string(), "1".to_string()),
        ];
        let resp = sandbox_respond(&req);
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            r#"{"x-a":"1","x-b":"2"}"#
        );
    }

    #[test]
    fn the_scripted_stream_head_carries_a_non_2xx_and_its_retry_hint() {
        // The case `FrameStream.status()` exists for: a rate limit whose BODY is a JSON document,
        // which under SSE framing decodes to nothing at all. Both halves are asserted together,
        // because either one alone would look fine.
        let (status, headers) = sandbox_stream_head(&get("https://x.test/stream/error"));
        assert_eq!(status, 429);
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(header("retry-after"), Some("30"));
        assert_eq!(header("content-type"), Some("application/json"));
        assert_eq!(
            crate::net::sandbox_stream_frames(
                &get("https://x.test/stream/error"),
                noeta_ext_abi::stream::Framing::Sse,
            ),
            vec![],
            "a JSON error document is not an event stream, so SSE cuts it into zero frames"
        );
    }

    #[test]
    fn the_stream_head_shares_the_buffered_status_grammar() {
        // One status table, not two: `/status/{n}` means the same thing streamed as buffered.
        for path in ["/status/503", "/status/204", "/echo", "/anything"] {
            let request = get(&format!("https://x.test{path}"));
            assert_eq!(
                sandbox_stream_head(&request).0,
                sandbox_respond(&request).status,
                "{path}"
            );
        }
    }

    #[test]
    fn default_route_echoes_method_and_path_as_text() {
        let resp = sandbox_respond(&get("https://api.example.com/v1/things"));
        assert_eq!(resp.status, 200);
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            "noeta sandbox: GET /v1/things"
        );
    }
}
