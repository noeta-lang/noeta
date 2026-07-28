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
/// - anything else → a single-frame body, so a stream against any URL still terminates.
pub fn sandbox_stream_body(request: &NetRequest) -> String {
    match path_of(&request.url) {
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
        path => format!("data: noeta sandbox: {} {path}\n\n", request.method),
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

/// The path of `url` — between the authority and any `?`/`#` — defaulting to `/`. A minimal
/// hand parse so the sandbox stays dependency-free (the `url` crate lives only in the real host's
/// reqwest tree).
fn path_of(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let from_path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => "/",
    };
    let end = from_path.find(['?', '#']).unwrap_or(from_path.len());
    &from_path[..end]
}

/// The deterministic sandbox response for `request` — the whole `Network` capability on
/// [`crate::SandboxHost`]. Pure, so both backends compute the identical response and the
/// differential holds.
///
/// Control grammar (by request path):
/// - `/status/{n}` → an empty response with status `n` (a malformed or out-of-range `n` is `400`).
/// - `/echo` → `200`, JSON body `{method, path, body}` echoing the request.
/// - `/headers` → `200`, JSON body of the request headers (sorted by name).
/// - anything else → `200`, the plain-text line `noeta sandbox: {method} {path}`.
pub fn sandbox_respond(request: &NetRequest) -> NetResponse {
    // Every arm builds its response through `typed`, which cannot know the request; stamp the
    // originating URL once here so a sandbox response carries it exactly as a real one does.
    let mut response = sandbox_body(request);
    response.url = request.url.clone();
    response
}

fn sandbox_body(request: &NetRequest) -> NetResponse {
    let path = path_of(&request.url);
    if let Some(rest) = path.strip_prefix("/status/") {
        return match rest.parse::<u16>() {
            Ok(n) if (100..=599).contains(&n) => typed(n, "text/plain", ""),
            _ => typed(400, "text/plain", "invalid status"),
        };
    }
    match path {
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
        }
    }

    #[test]
    fn path_parsing_strips_scheme_authority_query_and_fragment() {
        assert_eq!(path_of("https://x.test/a/b?q=1#f"), "/a/b");
        assert_eq!(path_of("http://x.test"), "/");
        assert_eq!(path_of("https://x.test/status/404"), "/status/404");
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
    fn default_route_echoes_method_and_path_as_text() {
        let resp = sandbox_respond(&get("https://api.example.com/v1/things"));
        assert_eq!(resp.status, 200);
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            "noeta sandbox: GET /v1/things"
        );
    }
}
