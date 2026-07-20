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

    /// Resolve a request target against the base URL.
    ///
    /// An **absolute** target (one with a scheme) wins outright — that is what lets a paginator
    /// follow an absolute `Link`/`next` URL through a client that has a base. Otherwise the target
    /// is joined to the base with exactly one `/` between them. With no base configured, the
    /// target is used as given (so a `Client` with no base behaves like the free verbs).
    pub fn resolve(&self, target: &str) -> String {
        if target.contains("://") || self.base_url.is_empty() {
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
        }
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

    #[test]
    fn basic_auth_encodes_per_rfc_7617() {
        // The RFC's own example: `Aladdin:open sesame`.
        assert_eq!(
            basic_auth_value("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }
}
