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

use crate::cookie_jar::CookieJar;
use crate::signature::SigningKey;
use crate::{ExternValue, NetRequest};
use std::any::Any;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

/// The registered extern-type name of a configured client.
pub const CLIENT_TYPE_NAME: &str = "Client";

/// `Client`'s qualified runtime identity — the `Response`/`HttpError` twin.
pub const CLIENT_TYPE_IDENTITY: &str = "std.http.Client";

/// A configured HTTP client: base URL + headers + deadline, spent by the verb methods.
///
/// Configuration is pure, content-equal data (no host handle, no connection pool — pooling is the
/// host's, keyed by origin, and outlives any one client value). Cloning is cheap enough that the
/// immutable-builder chain is not a performance concern: a chain of N steps allocates N small
/// header vectors once, at configuration time, not per request.
///
/// The **cookie jar** is the one exception, and it has to be: a jar is what a client *learns*, and
/// learning is not immutable. It is therefore shared — `client.cookies().bearer(token)` derives a
/// second client that keeps the first one's jar rather than starting an empty one, which is the
/// only behavior that makes a configuration chain usable after a login. Two clients holding the
/// same jar compare equal on it; two holding separate jars do not, whatever is in them, because a
/// jar is an identity rather than a value — the next response separates them.
#[derive(Debug, Clone, Default)]
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
    /// The cookie jar shared with every client derived from the one that created it, or `None`
    /// for a client that neither stores nor sends cookies.
    pub jar: Option<Arc<Mutex<CookieJar>>>,
    /// The RFC 9421 key every request is signed with, or `None` to send requests unsigned.
    pub signing: Option<SigningKey>,
}

/// Configuration compares by value; the jar compares by **identity** (see [`HttpClient`]).
impl PartialEq for HttpClient {
    fn eq(&self, other: &HttpClient) -> bool {
        self.base_url == other.base_url
            && self.headers == other.headers
            && self.timeout_ms == other.timeout_ms
            && self.retry == other.retry
            && self.redirect_limit == other.redirect_limit
            && self.signing == other.signing
            && match (&self.jar, &other.jar) {
                (None, None) => true,
                (Some(mine), Some(theirs)) => Arc::ptr_eq(mine, theirs),
                _ => false,
            }
    }
}

impl Eq for HttpClient {}

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

    /// A copy with a **fresh, empty** cookie jar. Every client derived from the result shares it,
    /// so a login performed through one is carried by the next request through any of them.
    ///
    /// A no-op on a client that already has one: calling it twice would otherwise discard the
    /// session the first call collected, and `client.cookies()` reads as "I want cookies", not
    /// "start over".
    pub fn with_cookies(&self) -> HttpClient {
        if self.jar.is_some() {
            return self.clone();
        }
        HttpClient {
            jar: Some(Arc::new(Mutex::new(CookieJar::new()))),
            ..self.clone()
        }
    }

    /// A copy that signs every request it sends with `key` (RFC 9421).
    pub fn with_signing(&self, key: SigningKey) -> HttpClient {
        HttpClient {
            signing: Some(key),
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

    /// Every live cookie the jar holds at `now_ms`, in a request-independent order. Empty for a
    /// client without a jar.
    pub fn stored_cookies(&self, now_ms: u64) -> Vec<crate::cookie::Cookie> {
        let Some(jar) = &self.jar else {
            return Vec::new();
        };
        let jar = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        jar.snapshot(now_ms)
            .into_iter()
            .map(|entry| entry.cookie)
            .collect()
    }

    /// Seed the jar with `cookie`, as if `url` had set it. `false` when the client has no jar, or
    /// when the cookie's own domain does not cover `url`'s host.
    pub fn store_cookie(&self, url: &str, cookie: crate::cookie::Cookie, now_ms: u64) -> bool {
        let Some(jar) = &self.jar else {
            return false;
        };
        let mut jar = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        jar.store(url, &cookie.to_header(), now_ms)
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
        // The origin the caller actually asked for, captured before the chain can move it. A
        // signature is not applied past it — see `hop`.
        let origin = noeta_ext_abi::uri::origin_of(&request.url);
        noeta_ext_abi::redirect::follow_redirects(request, |hop| self.hop(hop, &origin, host))
    }

    /// One hop: apply what this client adds to a request, perform it, and take in what the
    /// response teaches the client.
    ///
    /// This is per **hop** rather than per request, and that placement is the whole reason
    /// [`noeta_ext_abi::redirect::follow_redirects`] takes a closure. A cookie set by hop 1 has to
    /// be sent on hop 2 — a login that answers `302` and a `Set-Cookie` in the same response is
    /// the single most common shape there is, and applying the jar once per request would send
    /// the session to the login page and nowhere after it.
    fn hop(
        &self,
        request: NetRequest,
        origin: &str,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NetResponse, crate::NetError> {
        let jar = self.jar.clone();
        if jar.is_none() && self.signing.is_none() {
            return host.net_fetch(request);
        }
        // Read the wall clock once per hop. The sandbox's is a pure read that does not advance its
        // logical time, so a client with a jar observes the same clock a client without one does —
        // which is what keeps every existing timing case bit-identical.
        let now_ms = host.clock_unix_ms();
        let mut request = request;
        if let Some(jar) = &jar {
            let stored = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(header) = stored.header_for(&request.url, now_ms) {
                // A `Cookie` header set on the call or the client wins: the caller named it, and
                // silently merging the jar into it would produce a header neither side wrote.
                if !request
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                {
                    request.headers.push(("cookie".to_string(), header));
                }
            }
        }
        // Signing comes last, and per hop, for two reasons that are really one: a signature covers
        // a specific method and target, so a redirect invalidates it, and it may cover the
        // `Cookie` header the jar just added. Signing before either would sign a request that is
        // not the one going out.
        //
        // And it stops at the origin the caller named. A redirect can point anywhere, and signing
        // whatever it points at would hand a third party a valid signature under our key over a
        // request they chose the shape of. The redirect layer has already stripped the previous
        // hop's signature crossing that boundary; not minting a new one is the other half of the
        // same rule.
        if let Some(key) = self
            .signing
            .as_ref()
            .filter(|_| noeta_ext_abi::uri::origin_of(&request.url) == origin)
        {
            crate::signature::sign_request(&mut request, key, (now_ms / 1_000) as i64).map_err(
                |error| {
                    crate::NetError::new(
                        crate::NetErrorKind::Other,
                        &request.url,
                        format!("the request could not be signed: {}", error.message),
                    )
                },
            )?;
        }
        let sent_to = request.url.clone();
        let response = host.net_fetch(request)?;
        // The response's own URL is where the `Set-Cookie` was actually served from, which after a
        // hop is not the URL this request started at. A host that leaves it empty falls back.
        let from = match response.url.is_empty() {
            true => sent_to.as_str(),
            false => response.url.as_str(),
        };
        if let Some(jar) = &jar {
            jar.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .store_response(from, &response.headers, now_ms);
        }
        Ok(response)
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

    /// A derived client keeps the jar it came from.
    ///
    /// This is the whole reason the jar is shared rather than copied: a configuration chain is
    /// normally written *after* a login (`session.bearer(token).timeout(500)`), and a jar that
    /// copied would leave the derived client logged out with nothing to show for it.
    #[test]
    fn a_derived_client_carries_the_jar_it_came_from() {
        let session = HttpClient::new("https://x.test").with_cookies();
        session.store_cookie("https://x.test/", cookie("sid", "abc"), 0);

        let derived = session
            .with_header("accept", "application/json")
            .with_timeout(500);
        assert_eq!(
            derived.stored_cookies(0).len(),
            1,
            "configuring a client further must not log it out"
        );

        // And it is genuinely the same jar, not an equal copy: what the derived client learns is
        // visible to the one it came from.
        derived.store_cookie("https://x.test/", cookie("theme", "dark"), 0);
        assert_eq!(session.stored_cookies(0).len(), 2);
    }

    #[test]
    fn a_jar_compares_by_identity_and_configuration_by_value() {
        let a = HttpClient::new("https://x.test").with_cookies();
        assert_eq!(
            a,
            a.clone(),
            "a clone shares the jar, so it is the same client"
        );
        assert_eq!(
            a.with_header("x", "1"),
            a.with_header("x", "1"),
            "two derivations of one client agree on configuration and share one jar"
        );

        // Two clients with separate jars are not equal even while the jars hold the same thing —
        // the next response separates them, so treating them as one value would be a lie with a
        // short shelf life.
        let b = HttpClient::new("https://x.test").with_cookies();
        assert_ne!(a, b);
        assert_eq!(
            HttpClient::new("https://x.test"),
            HttpClient::new("https://x.test"),
            "and two jarless clients are still plain value-equal"
        );
    }

    #[test]
    fn asking_for_cookies_twice_does_not_empty_the_jar() {
        // `client.cookies()` reads as "I want cookies", not "start over" — and a chain that
        // mentions it twice must not silently discard the session the first mention collected.
        let session = HttpClient::new("https://x.test").with_cookies();
        session.store_cookie("https://x.test/", cookie("sid", "abc"), 0);
        assert_eq!(session.with_cookies().stored_cookies(0).len(), 1);
    }

    #[test]
    fn a_client_without_a_jar_stores_nothing_and_says_so() {
        let plain = HttpClient::new("https://x.test");
        assert!(!plain.store_cookie("https://x.test/", cookie("sid", "abc"), 0));
        assert!(plain.stored_cookies(0).is_empty());
    }

    /// **A cookie set on a redirecting response reaches the next hop.**
    ///
    /// The single most common shape on the web is a login that answers `302` *and* a
    /// `Set-Cookie` in the same response. A jar applied once per request rather than once per hop
    /// would store that cookie and then never send it — the session would be collected and
    /// immediately unused, which looks exactly like a server that ignored the login.
    #[test]
    fn a_cookie_set_by_a_redirect_rides_the_hop_it_redirects_to() {
        let mut host = crate::SandboxHost::new();
        let client = HttpClient::new("https://svc.test").with_cookies();
        let request = client.build("GET", "/cookies/login", Vec::new(), Vec::new());
        let response = client
            .perform(request, &mut host)
            .expect("the chain completes");

        assert_eq!(response.status, 200, "the 302 was followed");
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "{\"cookie\":\"session=live\"}",
            "the cookie the redirecting response set was sent on the hop it redirected to"
        );
    }

    #[test]
    fn the_jar_does_not_follow_a_cookie_across_an_origin() {
        // `/redirect-cross` hops to a second host. A jar keyed on the setting host must not send
        // the first host's cookie to the second, and the redirect layer strips the header the jar
        // would have added anyway — belt and braces, because either one alone leaking is a
        // session handed to whoever controls an open redirect.
        let mut host = crate::SandboxHost::new();
        let client = HttpClient::new("https://svc.test").with_cookies();
        client.store_cookie("https://svc.test/", cookie("sid", "abc"), 0);
        let request = client.build("GET", "/redirect-cross", Vec::new(), Vec::new());
        let response = client
            .perform(request, &mut host)
            .expect("the chain completes");

        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "{}",
            "nothing of ours may reach the other origin"
        );
    }

    /// **A signature does not follow a redirect off the caller's origin.**
    ///
    /// A `Location` can point anywhere. Signing whatever it points at would hand a third party a
    /// valid signature under our key over a request whose shape they chose — an oracle they had
    /// no business getting, for no benefit to anyone.
    #[test]
    fn a_signature_stops_at_the_origin_the_caller_named() {
        let mut host = crate::SandboxHost::new();
        let key = crate::signature::SigningKey::new("k1", b"secret");
        let client = HttpClient::new("https://svc.test").with_signing(key);

        // A same-origin hop is signed at its destination: `/redirect-same` lands on `/headers`,
        // which reports what arrived.
        let request = client.build("GET", "/redirect-same", Vec::new(), Vec::new());
        let arrived = String::from_utf8(
            client
                .perform(request, &mut host)
                .expect("the chain completes")
                .body,
        )
        .expect("json");
        assert!(
            arrived.contains("signature-input"),
            "a hop that stays put is still signed: {arrived}"
        );

        // A cross-origin hop is not — and carries nothing of the previous hop's either.
        let request = client.build("GET", "/redirect-cross", Vec::new(), Vec::new());
        let arrived = String::from_utf8(
            client
                .perform(request, &mut host)
                .expect("the chain completes")
                .body,
        )
        .expect("json");
        assert_eq!(
            arrived, "{}",
            "nothing signed by us may reach an origin the caller never named"
        );
    }

    /// A cookie for the tests above, panicking on an invalid one (they all name valid ones).
    fn cookie(name: &str, value: &str) -> crate::cookie::Cookie {
        crate::cookie::Cookie::new(name, value).expect("a valid cookie")
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
