//! `Cookie` — the typed `Set-Cookie` builder and the `Cookie:` request-header parser.
//!
//! Cookies are the one HTTP header that a program is *guaranteed* to build from untrusted input:
//! the value is almost always derived from user identity. That makes string concatenation the
//! wrong tool twice over — a `\r\n` in a name or value splits the response and lets the caller
//! append headers of their choosing (response splitting), and a stray `;` silently truncates the
//! cookie into attributes the author never wrote. So this type validates at **construction**
//! ([`Cookie::new`], the `with_*` builders) rather than at serialization: an invalid cookie
//! cannot be built, which means [`Cookie::to_header`] is total and no caller has to remember to
//! check anything.
//!
//! The shape is [`crate::net::NetResponse`]'s — pure, content-equal, immutable, so every builder
//! returns a new value. It reaches no host and crosses no seam, so unlike `NetResponse` it lives
//! here in the stdlib rather than in the ABI crate.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::ExternValue;
use noeta_ext_abi::{ErrorKind, StdError};

/// The registered extern-type name.
pub const COOKIE_TYPE_NAME: &str = "Cookie";

/// `Cookie`'s qualified runtime identity — the [`crate::net::RESPONSE_TYPE_IDENTITY`] twin.
pub const COOKIE_TYPE_IDENTITY: &str = "std.http.Cookie";

/// The `SameSite` attribute — the cross-site request defence.
///
/// A closed set, spelled as a **string** at the language surface (`"lax"`, `"strict"`, `"none"`),
/// because the extern ABI has no enum door; [`HttpError::kind`](crate::net::HttpError) is the same
/// shape. [`SameSite::parse`] is what makes the string closed in practice — an unrecognised
/// spelling is an error at the call, not a silently dropped attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    /// The attribute spelling that goes on the wire.
    pub fn label(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }

    /// Parse the language-surface spelling, case-insensitively.
    pub fn parse(raw: &str) -> Result<SameSite, StdError> {
        match raw.to_ascii_lowercase().as_str() {
            "strict" => Ok(SameSite::Strict),
            "lax" => Ok(SameSite::Lax),
            "none" => Ok(SameSite::None),
            other => Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "`Cookie.with_same_site` takes \"strict\", \"lax\", or \"none\", got {other:?}"
                ),
            }),
        }
    }
}

/// A cookie: the `name=value` pair plus the attributes that decide who may read it back.
///
/// Built by `http.server.cookie(name, value)`, whose defaults are the safe ones — `Path=/`,
/// `HttpOnly`, `SameSite=Lax`. `Secure` defaults **off** so a plain-`http` localhost server works
/// out of the box; the signed-session layer above turns it on, because that is where the stakes
/// justify breaking local dev by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    pub domain: Option<String>,
    /// Lifetime in seconds. `0` is the canonical deletion form (see [`Cookie::expired`]); `None`
    /// omits the attribute, making it a session cookie the browser drops when it closes.
    pub max_age: Option<i64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: SameSite,
}

/// Reject a cookie **name** that is not an RFC 6265 token.
///
/// The separator set is RFC 2616's, which is what the cookie grammar defers to. Rejecting the
/// whole set rather than only the dangerous-looking members keeps this a whitelist in effect: any
/// byte outside printable ASCII is out, so control characters — `\r` and `\n` among them — cannot
/// reach the header.
fn validate_name(name: &str) -> Result<(), StdError> {
    if name.is_empty() {
        return Err(StdError {
            kind: ErrorKind::ArgType,
            message: "a cookie name must not be empty".to_string(),
        });
    }
    for byte in name.bytes() {
        let separator = matches!(
            byte,
            b'(' | b')'
                | b'<'
                | b'>'
                | b'@'
                | b','
                | b';'
                | b':'
                | b'\\'
                | b'"'
                | b'/'
                | b'['
                | b']'
                | b'?'
                | b'='
                | b'{'
                | b'}'
                | b' '
                | b'\t'
        );
        if !(0x21..=0x7e).contains(&byte) || separator {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "invalid byte {byte:#04x} in cookie name {name:?} — a name must be an RFC 6265 \
                     token (printable ASCII, no separators or whitespace)"
                ),
            });
        }
    }
    Ok(())
}

/// Reject a cookie **value** outside the RFC 6265 `cookie-value` set.
///
/// Whitespace, `,`, `;`, `"`, and `\` are excluded because each one either terminates the value or
/// changes how a parser reads it; control characters because of response splitting. A value that
/// needs any of them — arbitrary bytes, UTF-8 text — must be encoded first (base64url is the usual
/// choice, and what the session layer uses), which is a decision the caller should make explicitly
/// rather than have a silent escaping rule make for them.
fn validate_value(value: &str) -> Result<(), StdError> {
    for byte in value.bytes() {
        if !(0x21..=0x7e).contains(&byte) || matches!(byte, b'"' | b',' | b';' | b'\\') {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "invalid byte {byte:#04x} in cookie value — a value must be printable ASCII \
                     without whitespace, `\"`, `,`, `;`, or `\\`; encode it (base64url) first"
                ),
            });
        }
    }
    Ok(())
}

/// Reject an attribute value (`Path`, `Domain`) that could terminate the attribute or the header.
fn validate_attribute(what: &str, value: &str) -> Result<(), StdError> {
    for byte in value.bytes() {
        if byte < 0x20 || byte == 0x7f || matches!(byte, b';') {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "invalid byte {byte:#04x} in cookie {what} {value:?} — control characters and \
                     `;` would terminate the attribute"
                ),
            });
        }
    }
    Ok(())
}

impl Cookie {
    /// A cookie with the safe defaults: `Path=/`, `HttpOnly`, `SameSite=Lax`, no `Secure`.
    pub fn new(name: &str, value: &str) -> Result<Cookie, StdError> {
        validate_name(name)?;
        validate_value(value)?;
        Ok(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            path: Some("/".to_string()),
            domain: None,
            max_age: None,
            http_only: true,
            secure: false,
            same_site: SameSite::Lax,
        })
    }

    /// Replace the value, validating it exactly as [`Cookie::new`] did.
    pub fn with_value(&self, value: &str) -> Result<Cookie, StdError> {
        validate_value(value)?;
        Ok(Cookie {
            value: value.to_string(),
            ..self.clone()
        })
    }

    /// Replace `Path`.
    pub fn with_path(&self, path: &str) -> Result<Cookie, StdError> {
        validate_attribute("path", path)?;
        Ok(Cookie {
            path: Some(path.to_string()),
            ..self.clone()
        })
    }

    /// Replace `Domain`.
    pub fn with_domain(&self, domain: &str) -> Result<Cookie, StdError> {
        validate_attribute("domain", domain)?;
        Ok(Cookie {
            domain: Some(domain.to_string()),
            ..self.clone()
        })
    }

    /// Set `SameSite`.
    ///
    /// `SameSite=None` **implies `Secure`** and sets it: a browser rejects the combination
    /// outright, so the alternative to upgrading here is a cookie that is silently never stored —
    /// the single hardest cookie bug to diagnose, because the response looks correct on the wire.
    /// The upgrade is one-directional; clearing `Secure` afterwards is an explicit contradiction
    /// and [`Cookie::with_secure`] rejects it rather than quietly undoing this.
    pub fn with_same_site(&self, same_site: SameSite) -> Cookie {
        Cookie {
            same_site,
            secure: self.secure || same_site == SameSite::None,
            ..self.clone()
        }
    }

    /// Set `Secure`.
    ///
    /// Rejects `with_secure(false)` on a `SameSite=None` cookie — see [`Cookie::with_same_site`].
    pub fn with_secure(&self, secure: bool) -> Result<Cookie, StdError> {
        if !secure && self.same_site == SameSite::None {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: "`SameSite=None` requires `Secure` — a browser drops the cookie without \
                          it. Choose `with_same_site(\"lax\")` if this cookie must work over plain \
                          http."
                    .to_string(),
            });
        }
        Ok(Cookie {
            secure,
            ..self.clone()
        })
    }

    /// The deletion form: same name, empty value, `Max-Age=0`.
    ///
    /// Deleting a cookie means *overwriting* it with an expired one, and a browser only matches
    /// the overwrite when `Path` and `Domain` match the original — which is exactly why this is a
    /// method on an existing cookie rather than a free `delete(name)` function that could not
    /// know them.
    pub fn expired(&self) -> Cookie {
        Cookie {
            value: String::new(),
            max_age: Some(0),
            ..self.clone()
        }
    }

    /// Render the `Set-Cookie` header value.
    ///
    /// Total by construction — every component was validated on the way in, so there is no
    /// failure mode left here and no caller needs a `Result`.
    pub fn to_header(&self) -> String {
        let mut out = format!("{}={}", self.name, self.value);
        if let Some(path) = &self.path {
            out.push_str("; Path=");
            out.push_str(path);
        }
        if let Some(domain) = &self.domain {
            out.push_str("; Domain=");
            out.push_str(domain);
        }
        if let Some(max_age) = self.max_age {
            out.push_str("; Max-Age=");
            out.push_str(&max_age.to_string());
        }
        out.push_str("; SameSite=");
        out.push_str(self.same_site.label());
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        out
    }
}

/// Parse a `Cookie:` request header into its pairs, in header order.
///
/// Deliberately lenient where the request side has to be: a browser sends what it was given, and a
/// malformed pair from *some other* server's cookie must not cost the program the pairs it does
/// understand. So an entry without `=` is skipped rather than fatal. Values keep their surrounding
/// double quotes stripped (RFC 6265 permits a quoted form) but are otherwise returned verbatim —
/// decoding is the caller's, since only the caller knows what encoding it wrote.
pub fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

impl ExternValue for Cookie {
    fn type_identity(&self) -> &'static str {
        COOKIE_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Cookie>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        // The name only — a cookie's value is a credential often enough that interpolating one
        // into a log line or a panic message should not leak it.
        write!(out, "<cookie {}>", self.name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_safe_ones() {
        let cookie = Cookie::new("sid", "abc").expect("a plain pair is valid");
        assert_eq!(
            cookie.to_header(),
            "sid=abc; Path=/; SameSite=Lax; HttpOnly"
        );
    }

    #[test]
    fn crlf_cannot_reach_the_header() {
        // Response splitting: without validation this would append a header of the caller's
        // choosing to the response.
        assert!(Cookie::new("sid", "a\r\nX-Evil: 1").is_err());
        assert!(Cookie::new("si\r\nd", "a").is_err());
    }

    #[test]
    fn a_semicolon_cannot_forge_an_attribute() {
        // Without validation this would silently turn into a cookie with `HttpOnly` cleared.
        assert!(Cookie::new("sid", "abc; HttpOnly").is_err());
    }

    #[test]
    fn an_attribute_cannot_be_terminated() {
        let cookie = Cookie::new("sid", "abc").expect("valid");
        assert!(validate_attribute("path", "/ok").is_ok());
        assert!(validate_attribute("path", "/a; Domain=evil.test").is_err());
        let _ = cookie;
    }

    #[test]
    fn expired_keeps_path_and_domain() {
        // The overwrite only matches — and so only deletes — when these survive.
        let cookie = Cookie::new("sid", "abc").expect("valid").expired();
        assert_eq!(cookie.max_age, Some(0));
        assert_eq!(cookie.path.as_deref(), Some("/"));
    }

    #[test]
    fn same_site_none_cannot_be_left_insecure() {
        let none = Cookie::new("sid", "abc")
            .expect("valid")
            .with_same_site(SameSite::None);
        assert!(none.secure, "`None` must imply `Secure`");
        assert!(
            none.with_secure(false).is_err(),
            "clearing Secure would make the browser drop it silently"
        );
        assert!(none.to_header().contains("SameSite=None; Secure"));
    }

    #[test]
    fn parses_pairs_and_survives_a_malformed_neighbour() {
        let pairs = parse_cookie_header("a=1; junk; b=\"2\" ; =3; c=");
        assert_eq!(
            pairs,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), String::new()),
            ]
        );
    }
}
