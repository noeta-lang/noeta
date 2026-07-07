//! The `Network` capability's seam types and its deterministic sandbox responder (http arc H1) —
//! the [`crate::fs::Vfs`] analog for the network.
//!
//! A network has no program-visible "write" step, so unlike the Vfs there is no mutable store to
//! seed: the sandbox responder is a **pure function of the request**, deterministic by
//! construction, honoring a small httpbin-style control grammar so conformance can exercise every
//! response path and pin exact bytes. Under the sandbox every request — whatever its URL — is
//! answered here; a program that wants real data runs under `noeta run` (the real host).

use serde_json::json;

/// An outbound HTTP request crossing the [`crate::host::Network`] seam. Plain `Send` data (like
/// [`crate::ReadSource`]): the `http` dispatch builds it, whichever host runs it consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetRequest {
    /// The HTTP method, uppercased (`"GET"`, `"POST"`, …).
    pub method: String,
    /// The absolute request URL.
    pub url: String,
    /// Request headers in insertion order (name, value).
    pub headers: Vec<(String, String)>,
    /// The request body bytes — empty for a bodyless request.
    pub body: Vec<u8>,
}

/// An HTTP response crossing the [`crate::host::Network`] seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetResponse {
    /// The HTTP status code (e.g. `200`, `404`).
    pub status: u16,
    /// Response headers (name, value).
    pub headers: Vec<(String, String)>,
    /// The response body bytes.
    pub body: Vec<u8>,
}

impl NetResponse {
    /// A response with a single `content-type` header.
    fn typed(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> NetResponse {
        NetResponse {
            status,
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: body.into(),
        }
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
    let path = path_of(&request.url);
    if let Some(rest) = path.strip_prefix("/status/") {
        return match rest.parse::<u16>() {
            Ok(n) if (100..=599).contains(&n) => NetResponse::typed(n, "text/plain", ""),
            _ => NetResponse::typed(400, "text/plain", "invalid status"),
        };
    }
    match path {
        "/echo" => {
            let doc = json!({
                "method": request.method,
                "path": path,
                "body": String::from_utf8_lossy(&request.body),
            });
            NetResponse::typed(200, "application/json", doc.to_string())
        }
        "/headers" => {
            // BTreeMap → JSON object with keys sorted, so the body is stable regardless of header
            // insertion order.
            let sorted: std::collections::BTreeMap<&str, &str> = request
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            NetResponse::typed(200, "application/json", json!(sorted).to_string())
        }
        _ => NetResponse::typed(
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
