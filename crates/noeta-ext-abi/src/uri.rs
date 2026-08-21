//! The RFC 3986 URI arithmetic the HTTP layer needs: does this reference carry a scheme, what is
//! its origin, and what absolute URL does a relative reference resolve to against a base.
//!
//! It lives in the lean ABI crate because three layers need the *same* answers and must not drift:
//! the client resolves a request target against its base URL, redirect following resolves a
//! `Location` against the URL the response came from, and a cookie jar decides whether a stored
//! cookie's domain and path match the request. A second spelling of "is this the same origin"
//! is a security bug waiting for a cross-origin redirect to find it.
//!
//! Deliberately not the `url` crate: this crate is the lean seam every extension compiles against,
//! and the parsing needed here is a handful of `split_once` calls. Percent-encoding, IDNA and
//! userinfo parsing are *not* implemented — a URL is carried through as written, and only the
//! structural boundaries (scheme, authority, path, query) are found.

/// Whether `reference` is **absolute** — i.e. begins with a real scheme.
///
/// Deliberately not `contains("://")`: a *relative* path may legitimately carry a URL in its query
/// string (`/oauth/callback?redirect=https://app.example.com`), and treating that as absolute
/// drops the base and produces a bare relative string the host rejects. So the text before `://`
/// must itself be a valid RFC 3986 scheme — alphabetic first character, then alphanumerics / `+` /
/// `-` / `.` — which a path or query string never is.
pub fn has_scheme(reference: &str) -> bool {
    scheme_of(reference).is_some()
}

/// The scheme of an absolute URL, as written (callers compare case-insensitively), or `None` for a
/// relative reference.
pub fn scheme_of(url: &str) -> Option<&str> {
    let scheme = url.split_once("://").map(|(s, _)| s)?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// Everything after `scheme://` — authority plus path plus query — or the whole reference when it
/// has no scheme.
fn after_scheme(url: &str) -> &str {
    match scheme_of(url) {
        Some(scheme) => &url[scheme.len() + 3..],
        None => url,
    }
}

/// The authority (`userinfo@host:port`) of an absolute URL, empty for a relative reference.
pub fn authority_of(url: &str) -> &str {
    let rest = match scheme_of(url) {
        Some(scheme) => &url[scheme.len() + 3..],
        None => return "",
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &rest[..end]
}

/// The host of an absolute URL — the authority with any `userinfo@` and `:port` removed. An
/// IPv6 literal keeps its brackets, which is what a `Host` header carries and what a cookie
/// domain would have to match.
pub fn host_of(url: &str) -> &str {
    let authority = authority_of(url);
    let host = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    if let Some(end) = host.find(']') {
        // `[::1]:8080` — the port colon is the one after the bracket.
        return &host[..=end];
    }
    match host.rsplit_once(':') {
        Some((before, _)) => before,
        None => host,
    }
}

/// The explicit port of an absolute URL, or `None` when it rides the scheme's default.
pub fn port_of(url: &str) -> Option<u16> {
    let authority = authority_of(url);
    let host = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    let after_host = match host.find(']') {
        Some(end) => &host[end + 1..],
        None => host,
    };
    after_host
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
}

/// The path of a URL — between the authority and any `?`/`#` — defaulting to `/`.
pub fn path_of(url: &str) -> &str {
    let rest = after_scheme(url);
    let from_path = match scheme_of(url) {
        // Absolute: the path starts at the first `/` after the authority.
        Some(_) => match rest.find(['/', '?', '#']) {
            Some(i) if rest.as_bytes()[i] == b'/' => &rest[i..],
            _ => "/",
        },
        // Relative: the whole reference up to the query is the path.
        None => rest,
    };
    let end = from_path.find(['?', '#']).unwrap_or(from_path.len());
    match &from_path[..end] {
        "" => "/",
        path => path,
    }
}

/// The default port of a scheme, for origin comparison.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

/// Whether `url`'s scheme is the TLS-protected member of its pair — what a `Secure` cookie
/// requires and what decides whether credentials may ride the wire.
pub fn is_secure(url: &str) -> bool {
    scheme_of(url).is_some_and(|s| {
        let s = s.to_ascii_lowercase();
        s == "https" || s == "wss"
    })
}

/// The **origin** of an absolute URL, normalized: lowercase scheme, lowercase host, and the port
/// only when it differs from the scheme's default. `userinfo` is not part of an origin.
///
/// Normalizing matters because these strings come off the wire: `HTTPS://API.Example.com:443/x`
/// and `https://api.example.com/x` are the same origin, and a redirect that treated them as
/// different would strip an `Authorization` header for no reason (or, with the comparison the
/// other way round, forward one across a genuine origin change).
pub fn origin_of(url: &str) -> String {
    let scheme = scheme_of(url).unwrap_or_default().to_ascii_lowercase();
    let host = host_of(url).to_ascii_lowercase();
    match port_of(url).filter(|p| Some(*p) != default_port(&scheme)) {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

/// Whether two absolute URLs share a scheme, host and effective port.
pub fn same_origin(a: &str, b: &str) -> bool {
    origin_of(a) == origin_of(b)
}

/// `scheme://authority` of `url` exactly as written — the prefix a resolved reference is rebuilt
/// on. Unlike [`origin_of`] this keeps `userinfo` and a redundant port, because it is used to
/// *construct* a URL rather than to compare two.
fn origin_prefix(url: &str) -> String {
    match scheme_of(url) {
        Some(scheme) => format!("{scheme}://{}", authority_of(url)),
        None => String::new(),
    }
}

/// Split a reference at its first `?` into path and query.
fn split_query(reference: &str) -> (&str, Option<&str>) {
    match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    }
}

/// Drop a `#fragment`. An HTTP request never sends one, so it is removed at resolution rather
/// than carried into a request URL the host would have to strip again.
fn strip_fragment(reference: &str) -> &str {
    match reference.split_once('#') {
        Some((before, _)) => before,
        None => reference,
    }
}

/// RFC 3986 §5.2.4 `remove_dot_segments`, applied to a path alone.
///
/// Empty interior segments are **preserved** (`/a//b` stays `/a//b`): they are legal path
/// segments, and collapsing them would silently rewrite a target the server chose.
fn remove_dot_segments(path: &str) -> String {
    let absolute = path.starts_with('/');
    let body = match absolute {
        true => &path[1..],
        false => path,
    };
    let segments: Vec<&str> = body.split('/').collect();
    let last = segments.len().saturating_sub(1);
    let mut out: Vec<&str> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        match *segment {
            // A `.` or `..` in final position leaves a trailing slash behind (`a/.` → `a/`).
            "." => {
                if i == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == last {
                    out.push("");
                }
            }
            segment => out.push(segment),
        }
    }
    let joined = out.join("/");
    match absolute {
        true => format!("/{joined}"),
        false => joined,
    }
}

/// Merge a relative reference onto a base path per RFC 3986 §5.3: everything up to and including
/// the base's last `/`, then the reference.
fn merge_paths(base_path: &str, reference: &str) -> String {
    match base_path.rfind('/') {
        Some(i) => format!("{}{reference}", &base_path[..=i]),
        None => format!("/{reference}"),
    }
}

/// Resolve `reference` against `base` into an absolute URL (RFC 3986 §5.3).
///
/// `base` is expected to be absolute — for a redirect it is the URL the response actually came
/// from, which is what makes a relative `Location` resolve against the *current* hop rather than
/// the original request. A relative base is carried through as best it can be rather than failing:
/// the request would fail at the host anyway, with a message naming the URL.
pub fn resolve_reference(base: &str, reference: &str) -> String {
    let reference = strip_fragment(reference).trim();
    if has_scheme(reference) {
        return normalize(reference);
    }
    let base = strip_fragment(base);
    if let Some(rest) = reference.strip_prefix("//") {
        let scheme = scheme_of(base).unwrap_or("https");
        return normalize(&format!("{scheme}://{rest}"));
    }
    if reference.is_empty() {
        return base.to_string();
    }
    let prefix = origin_prefix(base);
    if let Some(query) = reference.strip_prefix('?') {
        return format!("{prefix}{}?{query}", path_of(base));
    }
    let (ref_path, ref_query) = split_query(reference);
    let merged = match ref_path.starts_with('/') {
        true => ref_path.to_string(),
        false => merge_paths(path_of(base), ref_path),
    };
    let path = remove_dot_segments(&merged);
    match ref_query {
        Some(query) => format!("{prefix}{path}?{query}"),
        None => format!("{prefix}{path}"),
    }
}

/// An absolute URL with its path's dot segments removed and an empty path filled in as `/`.
fn normalize(url: &str) -> String {
    let prefix = origin_prefix(url);
    let (path, query) = split_query(after_path_start(url));
    let path = remove_dot_segments(match path {
        "" => "/",
        path => path,
    });
    match query {
        Some(query) => format!("{prefix}{path}?{query}"),
        None => format!("{prefix}{path}"),
    }
}

/// The `path?query` tail of an absolute URL.
fn after_path_start(url: &str) -> &str {
    let rest = after_scheme(url);
    match rest.find(['/', '?']) {
        Some(i) => &rest[i..],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scheme_is_recognized_by_shape_not_by_substring() {
        assert!(has_scheme("https://x.example"));
        assert!(
            has_scheme("HTTP://x.example"),
            "schemes are case-insensitive"
        );
        assert!(
            has_scheme("x-custom.v2+json://host"),
            "RFC 3986 scheme chars"
        );
        assert!(!has_scheme("/a?u=https://x"), "a path is not a scheme");
        assert!(!has_scheme("://nohost"), "empty scheme");
        assert!(!has_scheme("1http://x"), "a scheme starts with a letter");
        assert!(!has_scheme("/plain/path"));
    }

    #[test]
    fn the_structural_boundaries_are_found() {
        let url = "https://user:pw@api.example.com:8443/v1/items?page=2#frag";
        assert_eq!(scheme_of(url), Some("https"));
        assert_eq!(authority_of(url), "user:pw@api.example.com:8443");
        assert_eq!(host_of(url), "api.example.com");
        assert_eq!(port_of(url), Some(8443));
        assert_eq!(path_of(url), "/v1/items");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_and_gives_up_its_port() {
        // The colon that separates host from port is the one AFTER the closing bracket — a naive
        // `rsplit_once(':')` would cut the address in half and report `:1` as the host.
        assert_eq!(host_of("http://[::1]:8080/x"), "[::1]");
        assert_eq!(port_of("http://[::1]:8080/x"), Some(8080));
        assert_eq!(host_of("http://[::1]/x"), "[::1]");
        assert_eq!(port_of("http://[::1]/x"), None);
    }

    #[test]
    fn a_url_with_no_path_still_has_one() {
        assert_eq!(path_of("https://x.test"), "/");
        assert_eq!(path_of("https://x.test?q=1"), "/");
        assert_eq!(path_of("https://x.test#f"), "/");
    }

    #[test]
    fn an_origin_normalizes_case_and_the_default_port() {
        // These four are the same origin. A comparison that missed it would strip an
        // `Authorization` header across a redirect that never left the server.
        for url in [
            "https://API.Example.com/x",
            "https://api.example.com:443/x",
            "HTTPS://api.example.com/y?z=1",
            "https://user@api.example.com/x",
        ] {
            assert_eq!(origin_of(url), "https://api.example.com", "url={url}");
        }
    }

    #[test]
    fn a_different_scheme_host_or_port_is_a_different_origin() {
        let base = "https://api.example.com/x";
        for other in [
            "http://api.example.com/x",
            "https://api.example.org/x",
            "https://sub.api.example.com/x",
            "https://api.example.com:8443/x",
        ] {
            assert!(!same_origin(base, other), "other={other}");
        }
    }

    #[test]
    fn secure_is_the_tls_member_of_each_pair() {
        assert!(is_secure("https://x.test"));
        assert!(is_secure("WSS://x.test"));
        assert!(!is_secure("http://x.test"));
        assert!(!is_secure("ws://x.test"));
        assert!(!is_secure("/relative"));
    }

    #[test]
    fn a_reference_resolves_per_rfc_3986_section_5_4() {
        // The RFC's own normal-examples table, against its base.
        let base = "http://a/b/c/d;p?q";
        for (reference, expected) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("", "http://a/b/c/d;p?q"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("g;x", "http://a/b/c/g;x"),
            ("./../g", "http://a/b/g"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("http://x.test/y", "http://x.test/y"),
        ] {
            assert_eq!(
                resolve_reference(base, reference),
                expected,
                "reference={reference:?}"
            );
        }
    }

    #[test]
    fn a_fragment_never_reaches_the_request() {
        // An HTTP request has no fragment; resolving is where it goes, so no host has to strip it.
        assert_eq!(resolve_reference("http://a/b#one", "/c#two"), "http://a/c");
    }

    #[test]
    fn dot_segments_cannot_climb_above_the_root() {
        // `../../../../g` from a shallow base must not produce a path with `..` left in it, which
        // is how a traversal reaches a route the server never meant to expose.
        assert_eq!(
            resolve_reference("http://a/b/c", "../../../g"),
            "http://a/g"
        );
        assert!(!resolve_reference("http://a/b", "../../..").contains(".."));
    }

    #[test]
    fn an_empty_interior_segment_survives() {
        // `/a//b` is a legal path with an empty segment. Collapsing it would silently rewrite the
        // target the server chose in its `Location`.
        assert_eq!(resolve_reference("http://a/x", "/a//b"), "http://a/a//b");
    }

    #[test]
    fn a_relative_reference_resolves_against_the_current_hop() {
        // The redirect case: after one hop the base is where the response came from, so `next`
        // means "next relative to here", not relative to the original request.
        assert_eq!(
            resolve_reference("https://x.test/api/v2/list", "next"),
            "https://x.test/api/v2/next"
        );
    }
}
