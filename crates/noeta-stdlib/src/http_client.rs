//! The configured HTTP **client** (http arc H7) — `std.http.client`'s `Client` extern type.
//!
//! The free verbs (`client.get(url)`) are the one-shot door: no base URL, no shared headers, no
//! deadline. A `Client` is the *configured* door — the thing every other ecosystem calls a client
//! and PHP's api-toolkit calls a driver: base URL, headers applied to every request, an auth
//! scheme, and a timeout, bound once and spent many times.
//!
//! It is **immutable**, and every configuration method returns a new `Client` (the `with_header`
//! model `Response` already uses). That is what makes a chain safe to share: a derived client with
//! an extra header cannot mutate the one it came from, so a request-scoped tweak never leaks into
//! the client held by the rest of the program.
//!
//! Config lives here rather than in the ABI's [`crate::NetRequest`] because it never crosses the
//! Host seam: a verb call *expands* the client into a plain `NetRequest`, and the host only ever
//! sees the expanded result. The one exception is the deadline, which is genuinely per-request
//! and therefore rides the seam as `NetRequest::timeout_ms`.

use crate::{ExternValue, NetRequest};
use std::any::Any;
use std::cmp::Ordering;

/// The registered extern-type name of a configured client.
pub const CLIENT_TYPE_NAME: &str = "Client";

/// `Client`'s qualified runtime identity — the `Response`/`HttpError` twin.
pub const CLIENT_TYPE_IDENTITY: &str = "std.http.Client";

/// A configured HTTP client: base URL + headers + deadline, spent by the verb methods.
///
/// Pure, content-equal data (no host handle, no connection pool — pooling is the host's, keyed by
/// origin, and outlives any one client value). Cloning is cheap enough that the immutable-builder
/// chain is not a performance concern: a chain of N steps allocates N small header vectors once,
/// at configuration time, not per request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpClient {
    /// Prepended to any request path that is not itself absolute. Stored without a trailing `/`
    /// so joining is unambiguous.
    pub base_url: String,
    /// Headers applied to every request, in insertion order. A per-request header of the same
    /// name replaces the client's (see [`HttpClient::build`]).
    pub headers: Vec<(String, String)>,
    /// The per-request deadline in milliseconds, or `None` for the host's default.
    pub timeout_ms: Option<u64>,
    /// The retry policy, or `None` to attempt each request exactly once.
    pub retry: Option<RetryPolicy>,
    /// How many redirects a request follows, or `None` for
    /// [`noeta_ext_abi::redirect::DEFAULT_REDIRECT_LIMIT`]. `Some(0)` hands the 3xx back as an
    /// ordinary response.
    pub redirect_limit: Option<u32>,
}

/// How a [`HttpClient`] retries (http arc H9).
///
/// Two things are retried: a **transport** failure the seam classified as transient
/// ([`crate::NetErrorKind::retryable`] — timeout, dns, connect), and a **status** in
/// [`RetryPolicy::on_status`]. Nothing else: a TLS failure will not fix itself, and a `protocol`
/// failure may already have been applied server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// How many *additional* attempts after the first. `retry(3)` performs up to 4 requests.
    pub max_retries: u32,
    /// The first backoff, doubling per attempt (250, 500, 1000, …), capped at [`MAX_BACKOFF_MS`].
    pub base_ms: u64,
    /// Response statuses worth retrying. Defaults to [`DEFAULT_RETRY_STATUSES`].
    pub on_status: Vec<u16>,
    /// Whether to retry **non-idempotent** verbs (POST). Off by default — see
    /// [`RetryPolicy::should_retry_method`].
    pub non_idempotent: bool,
}

/// The statuses retried unless the caller names their own: 429 (rate limited — the case
/// `Retry-After` exists for) and the 502/503/504 gateway-and-overload trio. Deliberately NOT 500:
/// a generic server error is usually deterministic, and hammering it helps nobody.
pub const DEFAULT_RETRY_STATUSES: &[u16] = &[429, 502, 503, 504];

/// The first backoff when the caller does not name one.
pub const DEFAULT_BACKOFF_MS: u64 = 250;

/// The ceiling on a computed backoff. Exponential growth is useful for the first few attempts and
/// absurd after that — without a cap, `retry(10)` would wait minutes between attempts.
pub const MAX_BACKOFF_MS: u64 = 30_000;

impl RetryPolicy {
    /// A policy retrying `max_retries` times with the default backoff and status set.
    pub fn new(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_ms: DEFAULT_BACKOFF_MS,
            on_status: DEFAULT_RETRY_STATUSES.to_vec(),
            non_idempotent: false,
        }
    }

    /// Whether a request using `method` may be retried at all.
    ///
    /// The safety rule, and the reason this is not simply "retry what the policy says": retrying a
    /// **non-idempotent** request that may already have been applied can duplicate a side effect —
    /// a second charge, a second order. A timeout is exactly the case where the client cannot know
    /// whether the server processed it. So POST is not retried unless the caller explicitly opts
    /// in with `retry_non_idempotent()`, having decided their endpoint is safe (or that they send
    /// an idempotency key). Everything RFC 7231 defines as idempotent — GET, HEAD, PUT, DELETE,
    /// OPTIONS, TRACE — plus QUERY (safe and idempotent by its own definition) is retried freely.
    pub fn should_retry_method(&self, method: &str) -> bool {
        self.non_idempotent
            || matches!(
                method.to_ascii_uppercase().as_str(),
                "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "TRACE" | "QUERY"
            )
    }

    /// The backoff before attempt `retry` (0-based): `base * 2^retry`, capped.
    pub fn backoff_ms(&self, retry: u32) -> u64 {
        self.base_ms
            .saturating_mul(1u64 << retry.min(20))
            .min(MAX_BACKOFF_MS)
    }

    /// How long to wait before the next attempt, preferring the server's own `Retry-After` over
    /// our computed backoff when the response carries one.
    ///
    /// A server that says "wait 60s" knows something we do not, so it wins — but still capped, so
    /// a hostile or broken `Retry-After: 86400` cannot park the program for a day.
    pub fn delay_for(&self, retry: u32, response: Option<&crate::NetResponse>) -> u64 {
        response
            .and_then(|r| r.header_value("retry-after"))
            .and_then(parse_retry_after_seconds)
            .map(|secs| secs.saturating_mul(1000).min(MAX_BACKOFF_MS))
            .unwrap_or_else(|| self.backoff_ms(retry))
    }
}

/// Parse a `Retry-After` value as delay-seconds (RFC 9110 §10.2.3).
///
/// Only the delay-seconds form is honored; the HTTP-date form needs a wall-clock comparison and a
/// date parser, and servers that rate-limit overwhelmingly send seconds. An unparseable value
/// falls back to the computed backoff rather than failing the request.
fn parse_retry_after_seconds(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

impl HttpClient {
    /// A client rooted at `base_url` (empty for none). A trailing `/` is trimmed so that joining
    /// a path is a single unambiguous rule.
    pub fn new(base_url: &str) -> HttpClient {
        HttpClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            ..HttpClient::default()
        }
    }

    /// A copy with header `name: value` set, replacing any existing same-named header
    /// (case-insensitively, as HTTP header names are).
    pub fn with_header(&self, name: &str, value: &str) -> HttpClient {
        let mut next = self.clone();
        next.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        next.headers.push((name.to_string(), value.to_string()));
        next
    }

    /// A copy with the per-request deadline set.
    pub fn with_timeout(&self, ms: u64) -> HttpClient {
        HttpClient {
            timeout_ms: Some(ms),
            ..self.clone()
        }
    }

    /// A copy that follows at most `limit` redirects. `0` opts out entirely — the 3xx comes back
    /// as an ordinary response, `Location` header and all.
    pub fn with_redirect_limit(&self, limit: u32) -> HttpClient {
        HttpClient {
            redirect_limit: Some(limit),
            ..self.clone()
        }
    }

    /// A copy with the retry policy set.
    pub fn with_retry(&self, retry: RetryPolicy) -> HttpClient {
        HttpClient {
            retry: Some(retry),
            ..self.clone()
        }
    }

    /// A copy whose retry policy also covers non-idempotent verbs. A no-op without a policy —
    /// opting into retrying POST is meaningless if nothing is retried at all.
    pub fn with_non_idempotent_retry(&self) -> HttpClient {
        let mut next = self.clone();
        if let Some(retry) = next.retry.as_mut() {
            retry.non_idempotent = true;
        }
        next
    }

    /// Resolve a request target against the base URL.
    ///
    /// An **absolute** target (one with a scheme) wins outright — that is what lets a paginator
    /// follow an absolute `Link`/`next` URL through a client that has a base. Otherwise the target
    /// is joined to the base with exactly one `/` between them. With no base configured, the
    /// target is used as given (so a `Client` with no base behaves like the free verbs).
    pub fn resolve(&self, target: &str) -> String {
        if noeta_ext_abi::uri::has_scheme(target) || self.base_url.is_empty() {
            return target.to_string();
        }
        format!("{}/{}", self.base_url, target.trim_start_matches('/'))
    }

    /// Expand this client plus a call's own arguments into the request the host will perform.
    ///
    /// Header precedence is **call over client**: a per-request header replaces the client's
    /// same-named one rather than duplicating it, so a client-wide `accept` can be overridden for
    /// one call without rebuilding the client.
    pub fn build(
        &self,
        method: &str,
        target: &str,
        body: Vec<u8>,
        call_headers: Vec<(String, String)>,
    ) -> NetRequest {
        let mut headers = self.headers.clone();
        for (name, value) in call_headers {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            headers.push((name, value));
        }
        NetRequest {
            method: method.to_ascii_uppercase(),
            url: self.resolve(target),
            headers,
            body,
            timeout_ms: self.timeout_ms,
            redirect_limit: self.redirect_limit,
        }
    }

    /// Perform `request` through the host, applying this client's retry policy (http arc H9).
    ///
    /// Waiting between attempts goes through the **Clock** capability's `delay`, not a thread sleep
    /// written here: under the sandbox that advances the logical clock without blocking, so a
    /// retrying program stays deterministic and in-oracle and the differential covers this loop like
    /// any other code, while on a real host it elapses real time.
    ///
    /// It called `clock_sleep` — the *logical* advance — until the two intents were separated, which
    /// meant a shipped binary computed an exponential backoff, honoured a server's `Retry-After`,
    /// and then retried immediately anyway. Nothing failed; the policy was simply inert wherever it
    /// mattered.
    ///
    /// The **last** outcome is what the caller sees. A retry budget that runs out returns the
    /// final failure (or the final retryable status as an ordinary `Ok` response) rather than
    /// synthesizing a summary error — the caller asked for a request, not a report.
    pub fn perform(
        &self,
        request: NetRequest,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NetResponse, crate::NetError> {
        let Some(policy) = self.retry.as_ref().filter(|p| p.max_retries > 0) else {
            return self.attempt(request, host);
        };
        if !policy.should_retry_method(&request.method) {
            return self.attempt(request, host);
        }
        let mut retry = 0;
        loop {
            // The budget is checked BEFORE the attempt so the last one can consume the request
            // rather than clone it — a retrying client must not double-allocate a large body on
            // the common path where no retry actually happens.
            if retry >= policy.max_retries {
                return self.attempt(request, host);
            }
            let outcome = self.attempt(request.clone(), host);
            let delay = match &outcome {
                // A transient transport failure: worth another attempt. A deterministic one
                // (tls, invalid_url) or an ambiguous one (protocol, other) is returned as-is.
                Err(error) if error.kind.retryable() => policy.delay_for(retry, None),
                // A status the caller nominated. The server's own `Retry-After` wins if present.
                Ok(response) if policy.on_status.contains(&response.status) => {
                    policy.delay_for(retry, Some(response))
                }
                _ => return outcome,
            };
            host.clock_delay(delay as i64);
            retry += 1;
        }
    }

    /// One **attempt**: the redirect chain, start to finish.
    ///
    /// Retries wrap redirects rather than the other way round, which is the only nesting that
    /// makes sense from the caller's side. A retry means "that request did not work, do the whole
    /// thing again" — starting from the original target, because the chain that led anywhere else
    /// is exactly what is being retried. A hop that fails mid-chain therefore replays from hop
    /// zero, and the retry budget counts whole requests, not hops.
    fn attempt(
        &self,
        request: NetRequest,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NetResponse, crate::NetError> {
        noeta_ext_abi::redirect::follow_redirects(request, |hop| host.net_fetch(hop))
    }
}

/// `HttpClient` IS the user-facing `Client` extern type — pure, content-equal, not key-capable,
/// the [`crate::NetResponse`] model.
impl ExternValue for HttpClient {
    fn type_identity(&self) -> &'static str {
        CLIENT_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<HttpClient>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        match self.base_url.is_empty() {
            true => write!(out, "<client>"),
            false => write!(out, "<client {}>", self.base_url),
        }
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Parse an RFC 8288 `Link` header value into `rel -> target` pairs.
///
/// This is a **standard**, not one API's convention: `Link` is an IANA-registered header used by
/// GitHub, GitLab, Jira, Shopify, WordPress and others. Keeping the parse here as a small,
/// independently useful primitive (`resp.links()`) — rather than burying it inside a paginator —
/// is what lets the `Link` pagination strategy be one of several rather than the privileged one.
///
/// The grammar handled is the one servers actually emit:
/// `<url>; rel="next", <url>; rel="prev"`. Parameters other than `rel` are ignored, `rel` may be
/// quoted or bare, and a multi-valued `rel` (`rel="next last"`) registers the target under each
/// relation. First occurrence of a relation wins. A malformed element is skipped rather than
/// failing the whole header — a paginator should not die because a server appended junk.
pub fn parse_link_header(value: &str) -> Vec<(String, String)> {
    let mut links: Vec<(String, String)> = Vec::new();
    for element in split_link_elements(value) {
        let element = element.trim();
        // `<target>` must open the element; anything else is not a link-value.
        let Some(rest) = element.strip_prefix('<') else {
            continue;
        };
        let Some((target, params)) = rest.split_once('>') else {
            continue;
        };
        for param in params.split(';') {
            let Some((name, raw)) = param.split_once('=') else {
                continue;
            };
            if !name.trim().eq_ignore_ascii_case("rel") {
                continue;
            }
            let value = raw.trim().trim_matches('"');
            for rel in value.split_whitespace() {
                if !links.iter().any(|(existing, _)| existing == rel) {
                    links.push((rel.to_string(), target.trim().to_string()));
                }
            }
        }
    }
    links
}

/// Split a `Link` header on the commas that separate elements — the ones **outside** `<...>`.
/// A target URL may legally contain a comma (`?ids=1,2`), so a naive `split(',')` would tear an
/// element in half.
fn split_link_elements(value: &str) -> Vec<&str> {
    let mut elements = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in value.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                elements.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    elements.push(&value[start..]);
    elements
}

/// The `Authorization` value for HTTP Basic per RFC 7617: `Basic base64(user:pass)`.
pub fn basic_auth_value(user: &str, password: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_joins_with_exactly_one_slash() {
        // Whatever combination of trailing/leading slashes the caller supplies.
        for (base, path) in [
            ("https://api.example.com", "/users"),
            ("https://api.example.com/", "/users"),
            ("https://api.example.com", "users"),
            ("https://api.example.com/", "users"),
        ] {
            assert_eq!(
                HttpClient::new(base).resolve(path),
                "https://api.example.com/users",
                "base={base} path={path}"
            );
        }
    }

    #[test]
    fn an_absolute_target_ignores_the_base() {
        // The paginator's case: a `next` link is absolute and must not be re-rooted.
        let client = HttpClient::new("https://api.example.com");
        assert_eq!(
            client.resolve("https://other.example.com/page/2"),
            "https://other.example.com/page/2"
        );
    }

    #[test]
    fn a_url_in_the_query_string_does_not_make_a_path_absolute() {
        // The regression: `contains("://")` treated these as absolute, dropped the base, and
        // produced a bare relative string the host rejects. Redirect/callback/webhook parameters
        // carrying a URL are ordinary.
        let client = HttpClient::new("https://api.example.com");
        for path in [
            "/oauth/callback?redirect=https://app.example.com",
            "/proxy?url=http://internal/health",
            "/search?q=how%20to%20write%20a://b",
        ] {
            assert_eq!(
                client.resolve(path),
                format!("https://api.example.com{path}"),
                "path={path}"
            );
        }
    }

    #[test]
    fn no_base_behaves_like_the_free_verbs() {
        assert_eq!(
            HttpClient::new("").resolve("https://x.example/a"),
            "https://x.example/a"
        );
    }

    #[test]
    fn a_call_header_replaces_the_clients_rather_than_duplicating() {
        let client = HttpClient::new("https://x.example")
            .with_header("accept", "application/json")
            .with_header("x-keep", "1");
        let request = client.build(
            "get",
            "/a",
            Vec::new(),
            vec![("Accept".to_string(), "text/plain".to_string())],
        );
        assert_eq!(
            request.headers,
            vec![
                ("x-keep".to_string(), "1".to_string()),
                ("Accept".to_string(), "text/plain".to_string()),
            ],
            "the call's `Accept` replaces the client's `accept`, case-insensitively"
        );
        assert_eq!(request.method, "GET", "the verb is uppercased");
    }

    #[test]
    fn configuration_is_immutable() {
        let base = HttpClient::new("https://x.example").with_header("a", "1");
        let derived = base.with_header("b", "2").with_timeout(500);
        assert_eq!(base.headers.len(), 1, "the parent is untouched");
        assert_eq!(base.timeout_ms, None);
        assert_eq!(derived.headers.len(), 2);
        assert_eq!(derived.timeout_ms, Some(500));
    }

    #[test]
    fn setting_a_header_twice_keeps_the_last() {
        let client = HttpClient::new("")
            .with_header("x", "1")
            .with_header("X", "2");
        assert_eq!(client.headers, vec![("X".to_string(), "2".to_string())]);
    }

    #[test]
    fn link_headers_parse_per_rfc_8288() {
        // GitHub's actual shape.
        let header = "<https://api.github.com/user/repos?page=2>; rel=\"next\", \
                      <https://api.github.com/user/repos?page=9>; rel=\"last\"";
        assert_eq!(
            parse_link_header(header),
            vec![
                (
                    "next".to_string(),
                    "https://api.github.com/user/repos?page=2".to_string()
                ),
                (
                    "last".to_string(),
                    "https://api.github.com/user/repos?page=9".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_comma_inside_a_target_does_not_split_the_element() {
        // The bug a naive `split(',')` would have: a legal comma in the query string.
        let header = "<https://x.example/items?ids=1,2,3>; rel=\"next\"";
        assert_eq!(
            parse_link_header(header),
            vec![(
                "next".to_string(),
                "https://x.example/items?ids=1,2,3".to_string()
            )]
        );
    }

    #[test]
    fn rel_may_be_bare_multi_valued_or_accompanied_by_other_params() {
        let header = "<https://x.example/a>; title=\"A\"; rel=next, <https://x.example/b>; rel=\"prev last\"";
        assert_eq!(
            parse_link_header(header),
            vec![
                ("next".to_string(), "https://x.example/a".to_string()),
                ("prev".to_string(), "https://x.example/b".to_string()),
                ("last".to_string(), "https://x.example/b".to_string()),
            ]
        );
    }

    #[test]
    fn a_malformed_element_is_skipped_not_fatal() {
        // A server appending junk must not cost the caller the links it did send.
        let header = "garbage, <https://x.example/a>; rel=\"next\", <no-close; rel=\"prev\"";
        assert_eq!(
            parse_link_header(header),
            vec![("next".to_string(), "https://x.example/a".to_string())]
        );
    }

    #[test]
    fn the_first_occurrence_of_a_relation_wins() {
        let header = "<https://x.example/1>; rel=\"next\", <https://x.example/2>; rel=\"next\"";
        assert_eq!(
            parse_link_header(header),
            vec![("next".to_string(), "https://x.example/1".to_string())]
        );
    }

    fn response_with(status: u16, headers: Vec<(&str, &str)>) -> crate::NetResponse {
        crate::NetResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
            url: String::new(),
        }
    }

    #[test]
    fn backoff_doubles_and_then_caps() {
        let policy = RetryPolicy::new(20);
        assert_eq!(policy.backoff_ms(0), 250);
        assert_eq!(policy.backoff_ms(1), 500);
        assert_eq!(policy.backoff_ms(2), 1_000);
        // Without the cap, attempt 20 would be 250ms << 20 ≈ 3 days.
        assert_eq!(policy.backoff_ms(20), MAX_BACKOFF_MS);
    }

    #[test]
    fn retry_after_wins_over_the_computed_backoff() {
        let policy = RetryPolicy::new(3);
        let response = response_with(429, vec![("retry-after", "5")]);
        assert_eq!(
            policy.delay_for(0, Some(&response)),
            5_000,
            "the server knows its own rate limit better than our backoff curve does"
        );
    }

    #[test]
    fn a_hostile_retry_after_is_still_capped() {
        let policy = RetryPolicy::new(3);
        let response = response_with(503, vec![("Retry-After", "86400")]);
        assert_eq!(
            policy.delay_for(0, Some(&response)),
            MAX_BACKOFF_MS,
            "a broken or hostile `Retry-After` must not park the program for a day"
        );
    }

    #[test]
    fn an_unparseable_retry_after_falls_back_to_the_backoff() {
        // The HTTP-date form, which we deliberately do not parse.
        let policy = RetryPolicy::new(3);
        let response = response_with(503, vec![("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(policy.delay_for(1, Some(&response)), policy.backoff_ms(1));
    }

    #[test]
    fn post_is_not_retried_unless_explicitly_opted_in() {
        let safe = RetryPolicy::new(3);
        assert!(
            !safe.should_retry_method("POST"),
            "may already have applied"
        );
        for method in ["GET", "HEAD", "PUT", "DELETE", "OPTIONS", "TRACE", "QUERY"] {
            assert!(safe.should_retry_method(method), "{method} is idempotent");
        }
        let opted_in = RetryPolicy {
            non_idempotent: true,
            ..RetryPolicy::new(3)
        };
        assert!(opted_in.should_retry_method("POST"));
    }

    #[test]
    fn the_default_status_set_excludes_500() {
        // 429 and the gateway trio are worth retrying; a generic 500 is usually deterministic.
        assert_eq!(DEFAULT_RETRY_STATUSES, &[429, 502, 503, 504]);
        assert!(!DEFAULT_RETRY_STATUSES.contains(&500));
    }

    #[test]
    fn opting_into_unsafe_retries_is_order_independent() {
        // `.retry(3).retry_non_idempotent()` and the reverse must agree — a chain's meaning
        // should not depend on the order of two independent configuration steps.
        let a = HttpClient::new("")
            .with_retry(RetryPolicy::new(3))
            .with_non_idempotent_retry();
        assert!(a.retry.as_ref().expect("policy").non_idempotent);
    }

    /// **A backoff has to reach the door that actually waits.**
    ///
    /// `perform` computed an exponential backoff, honoured a server's `Retry-After`, and then handed
    /// the result to `clock_sleep` — the *logical* advance. Under the sandbox that is exactly right
    /// and every existing test agreed; on a real host it meant a client configured to back off
    /// retried a rate-limited endpoint as fast as the socket allowed. Nothing failed. The policy was
    /// inert precisely where it mattered, and only there.
    ///
    /// So the assertion is about **which** capability call the loop makes, not about elapsed time:
    /// elapsed time is what the sandbox deliberately does not have, and a timing test here would be
    /// either slow or a coin flip. `clock_delay` is the host's real-time door — this pins that the
    /// retry loop goes through it, and that it does not go through `clock_sleep`.
    #[test]
    fn a_retry_backoff_waits_through_the_real_time_door() {
        /// The sandbox host with its clock observed. Everything else — including the scripted
        /// network this test drives through `/status/503` — is the real sandbox, so the loop under
        /// test is the one that ships.
        struct RecordingHost {
            base: crate::SandboxHost,
            /// Every `clock_sleep`, in order. Must stay empty: this loop wants real time.
            slept: Vec<i64>,
            /// Every `clock_delay`, in order — the backoffs the policy computed.
            delayed: Vec<i64>,
        }

        impl noeta_ext_abi::Clock for RecordingHost {
            fn clock_monotonic(&mut self) -> u64 {
                self.base.clock_monotonic()
            }
            fn clock_sleep(&mut self, ms: i64) {
                self.slept.push(ms);
                self.base.clock_sleep(ms);
            }
            fn clock_delay(&mut self, ms: i64) {
                self.delayed.push(ms);
                self.base.clock_delay(ms);
            }
            fn clock_unix_ms(&mut self) -> u64 {
                self.base.clock_unix_ms()
            }
        }

        noeta_ext_abi::delegate_host!(RecordingHost => base :
            FileReader, FileSystem, Rng, Console, Os, Env, Entropy, Ids, Network, P2pProvider,
            Cancellable, Tracing, Metrics, Logging);

        let policy = RetryPolicy::new(2);
        let expected: Vec<i64> = (0..policy.max_retries)
            .map(|r| policy.backoff_ms(r) as i64)
            .collect();

        let mut host = RecordingHost {
            base: crate::SandboxHost::new(),
            slept: Vec::new(),
            delayed: Vec::new(),
        };
        // The sandbox's scripted responder answers `/status/<n>` with that status — 503 is one of
        // the four the default policy retries.
        let client = HttpClient::new("").with_retry(policy);
        let request = client.build("GET", "http://x/status/503", Vec::new(), Vec::new());
        let response = client
            .perform(request, &mut host)
            .expect("a 503 is an ordinary response, not a transport error");

        assert_eq!(
            response.status, 503,
            "the last outcome is what the caller sees"
        );
        assert_eq!(
            host.delayed, expected,
            "each retry must wait the backoff the policy computed, through the real-time door"
        );
        assert!(
            host.slept.is_empty(),
            "the retry loop reached `clock_sleep` — the logical advance — so a shipped binary would \
             compute this backoff and then ignore it: {:?}",
            host.slept
        );
    }

    #[test]
    fn basic_auth_encodes_per_rfc_7617() {
        // The RFC's own example: `Aladdin:open sesame`.
        assert_eq!(
            basic_auth_value("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }
}
