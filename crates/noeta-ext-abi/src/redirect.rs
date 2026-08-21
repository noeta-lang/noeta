//! Redirect following (http arc): the one place that decides whether a 3xx becomes another
//! request, and what that request looks like.
//!
//! Following lives **above** the [`crate::host::Network`] seam rather than inside each host.
//! [`crate::host::Network::net_fetch`] performs exactly one hop — that is the seam's contract —
//! and every door that owns a whole request drives [`follow_redirects`] over it. Four hosts
//! implement the seam (the sandbox responder, reqwest, a `wasi:http` embedder hook, a browser's
//! `fetch`), and each has its own idea of what a redirect is; a rule spelled four times is four
//! chances for a cross-origin `Authorization` header to survive a hop on one platform and not on
//! another. Spelled once, the deterministic sandbox exercises it under the differential and every
//! platform inherits the result.
//!
//! The exception a reader will notice: an **async** descriptor follows internally, because there
//! is no synchronous caller above it that could. It uses [`redirect_target`] — the same decision —
//! so only the loop is written twice, never the policy.

use crate::net::{NetRequest, NetResponse};
use crate::uri;

/// How many hops a request follows when its caller names no limit. Ten is the long-standing
/// browser and library default, and it is comfortably above any legitimate chain — a genuine
/// redirect is one or two hops, and everything past that is a misconfiguration or a loop.
pub const DEFAULT_REDIRECT_LIMIT: u32 = 10;

/// The status codes that redirect. `300 Multiple Choices` has no single target and `304 Not
/// Modified` is a cache answer, so neither is followed.
pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Headers that describe a body, and are therefore wrong the moment the body is dropped.
const BODY_HEADERS: &[&str] = &[
    "content-length",
    "content-type",
    "content-encoding",
    "content-language",
    "content-location",
    "transfer-encoding",
    "content-digest",
    "digest",
];

/// Headers that must never survive a hop to a different origin. `authorization` is the obvious
/// one; `cookie` matters just as much, and a signature over the *previous* request's method and
/// target is worse than useless on the next one.
const CROSS_ORIGIN_HEADERS: &[&str] = &["authorization", "cookie", "signature", "signature-input"];

/// The request `response` redirects `request` to, or `None` when the chain ends here.
///
/// It ends when the status does not redirect, when there is no usable `Location`, or when `hops`
/// has reached the request's limit — in which case the caller keeps the 3xx **as an ordinary
/// response**. That is deliberately not an error: it is the same rule the retry budget follows
/// ("the last outcome is what the caller sees"), it is the only sane reading of `redirect(0)`
/// — a caller who asked not to follow wants the 3xx and its `Location`, not a failure — and a
/// exhausted budget is visible anyway, because a followed redirect never returns a 3xx.
///
/// The rewrite rules are RFC 9110 §15.4 as every client actually implements them:
///
/// - **303** — the caller is being told "look over there instead", so the next request is a `GET`
///   with no body. A `HEAD` stays a `HEAD`: it asked for headers and still wants only headers.
/// - **301 / 302** — a `POST` becomes a `GET` with no body. The RFC's own text warns that this is
///   what deployed clients do, and a server issuing a 301 after a form post is relying on it;
///   `PUT` and `DELETE` are carried through unchanged.
/// - **307 / 308** — method and body preserved exactly. That is the entire reason these two codes
///   were minted.
///
/// The `Location` resolves against the URL the response **came from**, not the URL originally
/// requested: after two hops a relative `Location` means "relative to where I am now".
pub fn redirect_target(
    request: &NetRequest,
    response: &NetResponse,
    hops: u32,
) -> Option<NetRequest> {
    if !is_redirect(response.status) {
        return None;
    }
    if hops >= request.redirect_limit.unwrap_or(DEFAULT_REDIRECT_LIMIT) {
        return None;
    }
    let location = response.header_value("location")?;
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    // The response stamps the URL it was served from; a host that left it empty (or a synthesized
    // response) falls back to the request's own URL.
    let base = match response.url.is_empty() {
        true => request.url.as_str(),
        false => response.url.as_str(),
    };
    let url = uri::resolve_reference(base, location);

    let method_is_head = request.method.eq_ignore_ascii_case("HEAD");
    let (method, body) = match response.status {
        303 if !method_is_head => ("GET".to_string(), Vec::new()),
        301 | 302 if request.method.eq_ignore_ascii_case("POST") => ("GET".to_string(), Vec::new()),
        _ => (request.method.clone(), request.body.clone()),
    };

    let mut headers = request.headers.clone();
    if body.is_empty() && !request.body.is_empty() {
        headers.retain(|(name, _)| !header_in(name, BODY_HEADERS));
    }
    if !uri::same_origin(base, &url) {
        headers.retain(|(name, _)| !header_in(name, CROSS_ORIGIN_HEADERS));
    }

    Some(NetRequest {
        method,
        url,
        headers,
        body,
        // Configuration rides the whole chain: a deadline is the caller's budget for the request,
        // not for one hop of it, and the limit is what bounds the chain at all.
        timeout_ms: request.timeout_ms,
        redirect_limit: request.redirect_limit,
    })
}

/// Whether `name` is one of `set`, compared case-insensitively as HTTP header names are.
fn header_in(name: &str, set: &[&str]) -> bool {
    set.iter().any(|known| name.eq_ignore_ascii_case(known))
}

/// Drive `fetch` — one hop of the network — until it produces a response that does not redirect.
///
/// `fetch` is a hook, not merely a transport: a configured client applies its cookie jar and its
/// request signature **inside** it, so both are recomputed per hop. That is the only correct
/// placement for either. A cookie set by hop 1 has to be sent on hop 2, and a signature covers a
/// specific method and target, so re-sending hop 1's signature on hop 2 would be a signature over
/// the wrong request.
pub fn follow_redirects<F>(
    request: NetRequest,
    mut fetch: F,
) -> Result<NetResponse, crate::NetError>
where
    F: FnMut(NetRequest) -> Result<NetResponse, crate::NetError>,
{
    let mut current = request;
    let mut hops = 0u32;
    loop {
        // Checked before the attempt so the final hop — and every hop of a `redirect(0)` client,
        // which is to say every request that opted out — consumes the request rather than cloning
        // a body it will never reuse.
        if hops >= current.redirect_limit.unwrap_or(DEFAULT_REDIRECT_LIMIT) {
            return fetch(current);
        }
        let carrier = current.clone();
        let response = fetch(current)?;
        match redirect_target(&carrier, &response, hops) {
            Some(next) => {
                current = next;
                hops += 1;
            }
            None => return Ok(response),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetErrorKind;

    fn request(method: &str, url: &str) -> NetRequest {
        NetRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: None,
            redirect_limit: None,
        }
    }

    fn redirect(status: u16, from: &str, location: &str) -> NetResponse {
        NetResponse {
            status,
            headers: vec![("location".to_string(), location.to_string())],
            body: Vec::new(),
            url: from.to_string(),
        }
    }

    fn ok(from: &str) -> NetResponse {
        NetResponse {
            status: 200,
            headers: Vec::new(),
            body: b"done".to_vec(),
            url: from.to_string(),
        }
    }

    #[test]
    fn only_the_five_redirecting_statuses_redirect() {
        for status in [301, 302, 303, 307, 308] {
            assert!(is_redirect(status), "{status} redirects");
        }
        // 300 has no single target and 304 is a cache answer; neither names where to go.
        for status in [200, 204, 300, 304, 400, 404, 500] {
            assert!(!is_redirect(status), "{status} does not redirect");
        }
    }

    #[test]
    fn a_relative_location_resolves_against_the_hop_it_came_from() {
        let first = request("GET", "https://x.test/a/b");
        // The response was served from `/a/b` after an earlier hop, so `c` means `/a/c`.
        let response = redirect(302, "https://x.test/a/b", "c");
        let next = redirect_target(&first, &response, 0).expect("a target");
        assert_eq!(next.url, "https://x.test/a/c");
    }

    #[test]
    fn a_post_becomes_a_bodyless_get_on_301_302_and_303() {
        for status in [301, 302, 303] {
            let mut first = request("POST", "https://x.test/submit");
            first.body = b"name=ada".to_vec();
            first.headers = vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("content-length".to_string(), "8".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            let response = redirect(status, "https://x.test/submit", "/done");
            let next = redirect_target(&first, &response, 0).expect("a target");
            assert_eq!(next.method, "GET", "status={status}");
            assert!(next.body.is_empty(), "status={status}");
            assert_eq!(
                next.headers,
                vec![("accept".to_string(), "application/json".to_string())],
                "a header describing a body that no longer exists must not survive (status={status})"
            );
        }
    }

    #[test]
    fn a_307_and_a_308_preserve_the_method_and_the_body() {
        // The entire reason these two codes were minted.
        for status in [307, 308] {
            let mut first = request("POST", "https://x.test/submit");
            first.body = b"name=ada".to_vec();
            first.headers = vec![("content-type".to_string(), "text/plain".to_string())];
            let response = redirect(status, "https://x.test/submit", "/done");
            let next = redirect_target(&first, &response, 0).expect("a target");
            assert_eq!(next.method, "POST", "status={status}");
            assert_eq!(next.body, b"name=ada", "status={status}");
            assert_eq!(next.headers, first.headers, "status={status}");
        }
    }

    #[test]
    fn a_303_leaves_a_head_as_a_head() {
        // A HEAD asked for headers and still wants only headers; turning it into a GET would
        // download a body nobody requested.
        let response = redirect(303, "https://x.test/a", "/b");
        let next =
            redirect_target(&request("HEAD", "https://x.test/a"), &response, 0).expect("a target");
        assert_eq!(next.method, "HEAD");
    }

    #[test]
    fn a_301_carries_an_idempotent_verb_through_unchanged() {
        for method in ["GET", "PUT", "DELETE", "HEAD"] {
            let mut first = request(method, "https://x.test/a");
            first.body = b"payload".to_vec();
            let response = redirect(301, "https://x.test/a", "/b");
            let next = redirect_target(&first, &response, 0).expect("a target");
            assert_eq!(next.method, method);
            assert_eq!(next.body, b"payload", "method={method}");
        }
    }

    #[test]
    fn credentials_do_not_survive_a_hop_to_another_origin() {
        // The one that matters: an open redirect on a trusted host is a routine finding, and a
        // client that forwards its bearer token to wherever the `Location` points hands the token
        // to whoever controls that parameter.
        let mut first = request("GET", "https://api.example.com/v1/thing");
        first.headers = vec![
            ("authorization".to_string(), "Bearer secret".to_string()),
            ("cookie".to_string(), "session=abc".to_string()),
            ("signature".to_string(), "sig1=:AAAA:".to_string()),
            (
                "signature-input".to_string(),
                "sig1=(\"@method\")".to_string(),
            ),
            ("accept".to_string(), "application/json".to_string()),
        ];
        let response = redirect(
            302,
            "https://api.example.com/v1/thing",
            "https://evil.example.net/collect",
        );
        let next = redirect_target(&first, &response, 0).expect("a target");
        assert_eq!(
            next.headers,
            vec![("accept".to_string(), "application/json".to_string())],
            "only the non-credential header may cross an origin boundary"
        );
    }

    #[test]
    fn credentials_do_survive_a_hop_within_the_same_origin() {
        // The other half: stripping on a same-origin hop would break every ordinary
        // `/login` → `/dashboard` redirect behind an `Authorization` header.
        let mut first = request("GET", "https://api.example.com/login");
        first.headers = vec![("authorization".to_string(), "Bearer secret".to_string())];
        let response = redirect(302, "https://api.example.com/login", "/dashboard");
        let next = redirect_target(&first, &response, 0).expect("a target");
        assert_eq!(next.headers, first.headers);
        // …including when the hop spells the origin differently.
        let response = redirect(
            302,
            "https://api.example.com/login",
            "HTTPS://API.example.com:443/dashboard",
        );
        let next = redirect_target(&first, &response, 0).expect("a target");
        assert_eq!(
            next.headers, first.headers,
            "the same origin, spelled loudly"
        );
    }

    #[test]
    fn a_3xx_without_a_usable_location_ends_the_chain() {
        let first = request("GET", "https://x.test/a");
        let mut bare = redirect(302, "https://x.test/a", "");
        assert!(redirect_target(&first, &bare, 0).is_none(), "empty");
        bare.headers.clear();
        assert!(redirect_target(&first, &bare, 0).is_none(), "absent");
    }

    #[test]
    fn the_limit_bounds_the_chain_and_zero_means_do_not_follow() {
        let mut first = request("GET", "https://x.test/a");
        let response = redirect(302, "https://x.test/a", "/b");
        first.redirect_limit = Some(0);
        assert!(redirect_target(&first, &response, 0).is_none(), "opted out");
        first.redirect_limit = Some(2);
        assert!(redirect_target(&first, &response, 0).is_some());
        assert!(redirect_target(&first, &response, 1).is_some());
        assert!(
            redirect_target(&first, &response, 2).is_none(),
            "the third hop is past a limit of two"
        );
    }

    #[test]
    fn configuration_rides_the_whole_chain() {
        // A deadline is the caller's budget for the request, not for one hop of it — resetting it
        // per hop would let a chain of ten hops take ten times the timeout the caller set.
        let mut first = request("GET", "https://x.test/a");
        first.timeout_ms = Some(500);
        first.redirect_limit = Some(3);
        let response = redirect(302, "https://x.test/a", "/b");
        let next = redirect_target(&first, &response, 0).expect("a target");
        assert_eq!(next.timeout_ms, Some(500));
        assert_eq!(next.redirect_limit, Some(3));
    }

    /// A scripted chain: `/a` → `/b` → `/c` → 200.
    fn chain(request: NetRequest) -> Result<NetResponse, crate::NetError> {
        Ok(match crate::uri::path_of(&request.url) {
            "/a" => redirect(302, &request.url, "/b"),
            "/b" => redirect(302, &request.url, "/c"),
            _ => ok(&request.url),
        })
    }

    #[test]
    fn the_loop_walks_a_chain_to_its_end() {
        let response = follow_redirects(request("GET", "https://x.test/a"), chain).expect("ok");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.url, "https://x.test/c",
            "the response reports where the body actually came from"
        );
    }

    #[test]
    fn an_exhausted_budget_hands_back_the_3xx_rather_than_failing() {
        // The retry budget's rule, applied here: the last outcome is what the caller sees. It is
        // also the only sane reading of `redirect(0)` — a caller who opted out wants the 3xx and
        // its `Location`, not an error.
        let mut first = request("GET", "https://x.test/a");
        first.redirect_limit = Some(1);
        let response = follow_redirects(first, chain).expect("ok");
        assert_eq!(response.status, 302);
        assert_eq!(response.header_value("location"), Some("/c"));
    }

    #[test]
    fn opting_out_performs_exactly_one_request() {
        let mut first = request("GET", "https://x.test/a");
        first.redirect_limit = Some(0);
        let mut hops = 0;
        let response = follow_redirects(first, |r| {
            hops += 1;
            chain(r)
        })
        .expect("ok");
        assert_eq!(hops, 1);
        assert_eq!(response.status, 302);
    }

    #[test]
    fn a_loop_terminates_at_the_default_limit() {
        // A server that redirects to itself must not park the program forever.
        let mut hops = 0;
        let response = follow_redirects(request("GET", "https://x.test/loop"), |r| {
            hops += 1;
            Ok(redirect(302, &r.url, "/loop"))
        })
        .expect("ok");
        assert_eq!(
            hops,
            DEFAULT_REDIRECT_LIMIT + 1,
            "the limit bounds the hops"
        );
        assert_eq!(response.status, 302);
    }

    #[test]
    fn a_transport_failure_mid_chain_is_returned_not_retried() {
        let outcome =
            follow_redirects(
                request("GET", "https://x.test/a"),
                |r| match crate::uri::path_of(&r.url) {
                    "/a" => Ok(redirect(302, &r.url, "/b")),
                    _ => Err(crate::NetError::new(
                        NetErrorKind::Dns,
                        &r.url,
                        "no such host",
                    )),
                },
            );
        let error = outcome.expect_err("the second hop fails");
        assert_eq!(error.kind, NetErrorKind::Dns);
        assert_eq!(
            error.url, "https://x.test/b",
            "the failing hop names itself"
        );
    }
}
