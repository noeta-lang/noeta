//! **HTTP Message Signatures** (RFC 9421) over the HMAC primitives in [`crate::crypto`].
//!
//! Signing a request answers a question a bearer token cannot: not "who are you" but "is *this*
//! request the one you sent". A token proves the caller had the token; a signature proves the
//! method, the target and the named headers were not altered on the way, and — with `created` and
//! a freshness window — that the request is not a replay of one captured an hour ago. That is what
//! every webhook receiver and every service-to-service call inside a mesh needs, and what the
//! `Authorization` header was never able to give.
//!
//! RFC 9421 rather than a house scheme, because "sign the request" is exactly the design where a
//! house scheme goes wrong quietly: the interesting failures are canonicalization disagreements —
//! whose idea of the path is right, how a repeated header combines, whether the query's `?`
//! belongs — and an interoperable answer to those is the entire content of the standard. The
//! §B.2.5 test vector is in this module's tests, so the implementation is checked against the
//! RFC's own bytes rather than against itself.
//!
//! **Covered by default:** `@method`, `@path`, `@query` and `@authority`. Not `@target-uri`, and
//! the reason is worth stating: an *outbound* request carries an absolute URL, while an *inbound*
//! one carries an origin-form target plus a `Host` header, so reconstructing one full URI on both
//! sides means guessing the scheme behind a proxy. The four defaults are exactly the parts both
//! sides can name without guessing, and together they cover the same ground.
//!
//! **The body is not covered unless you say so.** Name `"content-digest"` among the components and
//! the client computes RFC 9530's `Content-Digest: sha-256=:…:` over the body and signs it; leave
//! it out and a signature says nothing about the payload. There is no way to make that safe by
//! default — a digest over a body the caller is streaming cannot be computed at all — so it is a
//! choice the caller makes rather than one this module makes for them.

use base64::Engine;

use noeta_ext_abi::{ErrorKind, NetRequest, StdError};

/// The signature algorithms this module implements — the two HMAC entries of the RFC 9421
/// registry, which are the ones a shared secret can use. The asymmetric entries (`ed25519`,
/// `rsa-pss-sha512`, …) need a key pair and a key-distribution story, and neither is what a
/// program with a shared secret is reaching for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlg {
    HmacSha256,
    HmacSha512,
}

impl SignatureAlg {
    /// The registry identifier that goes in the `alg` parameter.
    pub fn label(self) -> &'static str {
        match self {
            SignatureAlg::HmacSha256 => "hmac-sha256",
            SignatureAlg::HmacSha512 => "hmac-sha512",
        }
    }

    /// Parse a registry identifier.
    pub fn parse(raw: &str) -> Result<SignatureAlg, StdError> {
        match raw {
            "hmac-sha256" => Ok(SignatureAlg::HmacSha256),
            "hmac-sha512" => Ok(SignatureAlg::HmacSha512),
            other => Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "unsupported signature algorithm {other:?} — this build signs and verifies \
                     \"hmac-sha256\" and \"hmac-sha512\""
                ),
            }),
        }
    }

    /// The tag over `data` under `key`.
    pub fn tag(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        match self {
            SignatureAlg::HmacSha256 => crate::crypto::hmac_sha256(key, data),
            SignatureAlg::HmacSha512 => crate::crypto::hmac_sha512(key, data),
        }
    }

    /// Whether `tag` is the tag over `data` under `key`, compared in constant time.
    pub fn verify(self, key: &[u8], data: &[u8], tag: &[u8]) -> bool {
        match self {
            SignatureAlg::HmacSha256 => crate::crypto::hmac_sha256_verify(key, data, tag),
            SignatureAlg::HmacSha512 => crate::crypto::hmac_sha512_verify(key, data, tag),
        }
    }
}

/// The components signed when a caller names none. See the module header for why `@target-uri` is
/// not among them.
pub const DEFAULT_COMPONENTS: &[&str] = &["@method", "@path", "@query", "@authority"];

/// The signature label used for a signature this client produces. RFC 9421 allows any label and
/// several at once; a client that produces exactly one has no use for the freedom, and a fixed
/// label is one less thing a receiver has to be told.
pub const DEFAULT_LABEL: &str = "sig1";

/// The `Content-Digest` component, spelled once so the "compute it if it is covered" rule and the
/// header it writes cannot drift apart.
pub const CONTENT_DIGEST: &str = "content-digest";

/// What a `Client` signs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningKey {
    /// The `keyid` parameter — how the receiver finds the secret to check against. Not itself a
    /// secret.
    pub key_id: String,
    /// The shared secret.
    pub secret: Vec<u8>,
    pub alg: SignatureAlg,
    /// The covered components, in the order they appear in the signature base.
    pub components: Vec<String>,
}

impl SigningKey {
    /// A key signing [`DEFAULT_COMPONENTS`] with HMAC-SHA256.
    pub fn new(key_id: &str, secret: &[u8]) -> SigningKey {
        SigningKey {
            key_id: key_id.to_string(),
            secret: secret.to_vec(),
            alg: SignatureAlg::HmacSha256,
            components: DEFAULT_COMPONENTS.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// A copy covering `components` instead of the defaults.
    pub fn with_components(&self, components: Vec<String>) -> SigningKey {
        SigningKey {
            components,
            ..self.clone()
        }
    }

    /// A copy signing with `alg`.
    pub fn with_alg(&self, alg: SignatureAlg) -> SigningKey {
        SigningKey {
            alg,
            ..self.clone()
        }
    }

    /// Whether the body's digest is covered, and therefore has to be computed and sent.
    pub fn covers_content_digest(&self) -> bool {
        self.components
            .iter()
            .any(|c| c.eq_ignore_ascii_case(CONTENT_DIGEST))
    }
}

/// The parameters of one signature: which components it covers and what was asserted about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignatureParams {
    /// The covered component identifiers, in signature-base order.
    pub components: Vec<String>,
    /// `created` — unix seconds. The freshness anchor: without it a captured signature is valid
    /// forever, which is the replay every signing scheme exists to stop.
    pub created: Option<i64>,
    /// `expires` — unix seconds, when the signer named a hard deadline of its own.
    pub expires: Option<i64>,
    /// `keyid` — which secret the receiver should check against.
    pub key_id: Option<String>,
    /// `alg` — the algorithm identifier, when the signer stated it.
    pub alg: Option<String>,
    /// `nonce` — an opaque single-use value, carried through and signed but not itself checked
    /// here: rejecting a repeated nonce needs storage that outlives a request, which is the
    /// receiver's to own.
    pub nonce: Option<String>,
    /// `tag` — an application-defined label for what this signature is for.
    pub tag: Option<String>,
}

impl SignatureParams {
    /// The `@signature-params` value: the covered-component list followed by the parameters, in
    /// the order RFC 9421's examples use. This exact string is both the last line of the signature
    /// base and the value of the `Signature-Input` header, which is what makes a verifier able to
    /// reconstruct the base from the header alone.
    pub fn to_value(&self) -> String {
        let components = self
            .components
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = format!("({components})");
        if let Some(created) = self.created {
            out.push_str(&format!(";created={created}"));
        }
        if let Some(expires) = self.expires {
            out.push_str(&format!(";expires={expires}"));
        }
        if let Some(key_id) = &self.key_id {
            out.push_str(&format!(";keyid=\"{key_id}\""));
        }
        if let Some(alg) = &self.alg {
            out.push_str(&format!(";alg=\"{alg}\""));
        }
        if let Some(nonce) = &self.nonce {
            out.push_str(&format!(";nonce=\"{nonce}\""));
        }
        if let Some(tag) = &self.tag {
            out.push_str(&format!(";tag=\"{tag}\""));
        }
        out
    }
}

/// The RFC 9530 `Content-Digest` value for `body`: `sha-256=:<base64>:`.
pub fn content_digest_value(body: &[u8]) -> String {
    let digest = crate::crypto::sha256(body);
    format!(
        "sha-256=:{}:",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// The value of one covered component, as it appears in the signature base.
///
/// `None` means the component is not present on this request — a field that was signed and is now
/// missing. That is a verification failure rather than an empty line, because treating an absent
/// header as empty would let anyone strip a covered header and keep the signature valid.
fn component_value(request: &NetRequest, component: &str) -> Option<String> {
    if let Some(derived) = component.strip_prefix('@') {
        return derived_component(request, derived);
    }
    // An HTTP field: every occurrence, in order, joined with `", "` (RFC 9421 §2.1), with the
    // surrounding whitespace of each value stripped. Repeating a header is semantically identical
    // to sending one comma-joined value, so a canonicalization that took only the first would
    // sign less than the receiver reads.
    let values: Vec<&str> = request
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(component))
        .map(|(_, value)| value.trim())
        .collect();
    match values.is_empty() {
        true => None,
        false => Some(values.join(", ")),
    }
}

/// The value of a **derived** component (RFC 9421 §2.2) — one describing the request itself rather
/// than one of its headers.
fn derived_component(request: &NetRequest, derived: &str) -> Option<String> {
    let url = request.url.as_str();
    match derived {
        "method" => Some(request.method.to_ascii_uppercase()),
        "target-uri" => Some(url.to_string()),
        "authority" => Some(authority_of(request)),
        "scheme" => Some(
            noeta_ext_abi::uri::scheme_of(url)
                .unwrap_or("https")
                .to_ascii_lowercase(),
        ),
        // The origin-form target as it goes on the request line: path plus query.
        "request-target" => Some(match query_of(url) {
            Some(query) => format!("{}?{query}", noeta_ext_abi::uri::path_of(url)),
            None => noeta_ext_abi::uri::path_of(url).to_string(),
        }),
        "path" => Some(noeta_ext_abi::uri::path_of(url).to_string()),
        // §2.2.7: the leading `?` is part of the value, and a request with no query signs a bare
        // `?` rather than nothing — so "no query" and "empty query" are distinguishable.
        "query" => Some(format!("?{}", query_of(url).unwrap_or_default())),
        // Anything else — `@status` (a response component), `@query-param` (which takes its own
        // parameter) — is not something this module can produce for a request.
        _ => None,
    }
}

/// The query string of `url` without its `?`, or `None` when there is none.
fn query_of(url: &str) -> Option<&str> {
    let after_fragment = url.split('#').next().unwrap_or(url);
    after_fragment.split_once('?').map(|(_, query)| query)
}

/// The `@authority` of a request: the `Host` header when there is one (an inbound request carries
/// the authority there and nowhere else), otherwise derived from the URL.
///
/// Both spellings have to agree or a signature made by a client never verifies at a server. They
/// do, because the `Host` header a client sends *is* the URL's authority with a default port
/// elided — which is exactly what the derivation below computes.
fn authority_of(request: &NetRequest) -> String {
    if let Some((_, host)) = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        return host.trim().to_ascii_lowercase();
    }
    let url = request.url.as_str();
    let host = noeta_ext_abi::uri::host_of(url).to_ascii_lowercase();
    let scheme = noeta_ext_abi::uri::scheme_of(url)
        .unwrap_or("https")
        .to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" | "ws" => Some(80),
        _ => Some(443),
    };
    match noeta_ext_abi::uri::port_of(url).filter(|p| Some(*p) != default_port) {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// Build the **signature base** (RFC 9421 §2.5) — the exact bytes that get signed.
///
/// One line per covered component, `"name": value`, then a final `"@signature-params"` line
/// carrying the parameters. Lines are joined with `\n` and there is **no** trailing newline; that
/// detail is not cosmetic, since a stray one changes every signature this module produces and
/// makes it interoperate with nothing.
pub fn signature_base(request: &NetRequest, params: &SignatureParams) -> Result<String, StdError> {
    let mut lines = Vec::with_capacity(params.components.len() + 1);
    for component in &params.components {
        let component = component.to_ascii_lowercase();
        let Some(value) = component_value(request, &component) else {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "cannot sign {component:?}: the request has no such component. A signed \
                     header must be present on the request that carries the signature."
                ),
            });
        };
        lines.push(format!("\"{component}\": {value}"));
    }
    lines.push(format!("\"@signature-params\": {}", params.to_value()));
    Ok(lines.join("\n"))
}

/// Sign `request` in place: adds `Content-Digest` when it is covered, then `Signature-Input` and
/// `Signature`.
///
/// `created` is unix **seconds**, and is what a verifier's freshness window is measured against.
pub fn sign_request(
    request: &mut NetRequest,
    key: &SigningKey,
    created: i64,
) -> Result<(), StdError> {
    // A covered digest has to exist before the base is built, or signing fails on its own header.
    if key.covers_content_digest() {
        let value = content_digest_value(&request.body);
        set_header(request, "content-digest", &value);
    }
    let params = SignatureParams {
        components: key.components.iter().map(|c| c.to_lowercase()).collect(),
        created: Some(created),
        key_id: Some(key.key_id.clone()),
        alg: Some(key.alg.label().to_string()),
        ..SignatureParams::default()
    };
    let base = signature_base(request, &params)?;
    let tag = key.alg.tag(&key.secret, base.as_bytes());
    set_header(
        request,
        "signature-input",
        &format!("{DEFAULT_LABEL}={}", params.to_value()),
    );
    set_header(
        request,
        "signature",
        &format!(
            "{DEFAULT_LABEL}=:{}:",
            base64::engine::general_purpose::STANDARD.encode(tag)
        ),
    );
    Ok(())
}

/// Replace any same-named header, then append — a request must not carry two `Signature` headers
/// after being signed twice (which a retried or redirected request is).
fn set_header(request: &mut NetRequest, name: &str, value: &str) {
    request
        .headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    request.headers.push((name.to_string(), value.to_string()));
}

/// Why a signature did not verify. The surface answers `bool` — a caller acting on a signature has
/// exactly one decision to make — but the reason is what a test can pin, and pinning it is how the
/// difference between "no signature at all" and "a signature that does not match" stays a real
/// difference rather than one both sides guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    /// No `Signature-Input` or no `Signature` header.
    Absent,
    /// The headers are present but do not parse, or the two disagree about the label.
    Malformed,
    /// The `alg` parameter names something this build does not implement.
    UnsupportedAlg,
    /// A covered component is missing from the request.
    MissingComponent,
    /// `created` is outside the freshness window, or `expires` has passed.
    Stale,
    /// Everything parsed and the tag does not match.
    Mismatch,
}

impl VerifyFailure {
    /// A short label for a diagnostic.
    pub fn label(self) -> &'static str {
        match self {
            VerifyFailure::Absent => "absent",
            VerifyFailure::Malformed => "malformed",
            VerifyFailure::UnsupportedAlg => "unsupported_alg",
            VerifyFailure::MissingComponent => "missing_component",
            VerifyFailure::Stale => "stale",
            VerifyFailure::Mismatch => "mismatch",
        }
    }
}

/// Verify the signature on `request` against `secret`.
///
/// `now` is unix seconds and `max_age` is the freshness window in seconds — a signature whose
/// `created` is older than that, or in the future by more than that, is rejected without the tag
/// even being computed. `None` disables the check, which is the right choice only when something
/// else stops replays (a nonce store, a transport that cannot be recorded).
///
/// The algorithm comes from the request's own `alg` parameter, and that is safe **only** because
/// the set is two HMAC variants over the same secret: there is no algorithm here an attacker could
/// name to make verification cheaper or trivial, which is the `alg: none` family of attacks. A
/// build that ever adds an asymmetric algorithm must stop trusting this parameter and take the
/// expected algorithm from the caller instead.
pub fn verify_request(
    request: &NetRequest,
    secret: &[u8],
    now: i64,
    max_age: Option<i64>,
) -> Result<(), VerifyFailure> {
    let input = header_value(request, "signature-input").ok_or(VerifyFailure::Absent)?;
    let signature = header_value(request, "signature").ok_or(VerifyFailure::Absent)?;
    let inputs = parse_signature_input(input);
    let signatures = parse_signature(signature);
    if inputs.is_empty() || signatures.is_empty() {
        return Err(VerifyFailure::Malformed);
    }
    // Any label that carries both an input and a signature is a candidate; one that verifies is
    // enough. A request may legitimately carry several signatures (a client's and a gateway's),
    // and only one of them is ours to check.
    let mut worst = VerifyFailure::Malformed;
    for (label, params) in &inputs {
        let Some((_, tag)) = signatures.iter().find(|(l, _)| l == label) else {
            continue;
        };
        match verify_one(request, secret, params, tag, now, max_age) {
            Ok(()) => return Ok(()),
            Err(failure) => worst = failure,
        }
    }
    Err(worst)
}

/// Verify one labelled signature.
fn verify_one(
    request: &NetRequest,
    secret: &[u8],
    params: &SignatureParams,
    tag: &[u8],
    now: i64,
    max_age: Option<i64>,
) -> Result<(), VerifyFailure> {
    let alg = match &params.alg {
        Some(alg) => SignatureAlg::parse(alg).map_err(|_| VerifyFailure::UnsupportedAlg)?,
        // A signer may omit `alg` and rely on the key telling the verifier which to use. With a
        // shared secret and two candidates, HMAC-SHA256 is the one to assume — it is the
        // overwhelmingly common choice and a wrong guess fails closed.
        None => SignatureAlg::HmacSha256,
    };
    if let Some(expires) = params.expires
        && now > expires
    {
        return Err(VerifyFailure::Stale);
    }
    if let Some(window) = max_age {
        let created = params.created.ok_or(VerifyFailure::Stale)?;
        // Both directions: a signature from the future is as suspect as one from last week, and a
        // clock ahead of ours is the ordinary reason for it.
        if (now - created).abs() > window {
            return Err(VerifyFailure::Stale);
        }
    }
    let base = signature_base(request, params).map_err(|_| VerifyFailure::MissingComponent)?;
    match alg.verify(secret, base.as_bytes(), tag) {
        true => Ok(()),
        false => Err(VerifyFailure::Mismatch),
    }
}

/// The first value of header `name`.
fn header_value<'a>(request: &'a NetRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Parse a `Signature-Input` header into its labelled parameter sets.
///
/// The grammar is RFC 8941 structured-field dictionary syntax; parsed directly rather than in
/// general, because the only member shape that appears here is `label=(components);params` and a
/// general structured-field parser would be a great deal of code serving one call site.
pub fn parse_signature_input(header: &str) -> Vec<(String, SignatureParams)> {
    let mut out = Vec::new();
    for member in split_dictionary(header) {
        let Some((label, value)) = member.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(close) = value.find(')') else {
            continue;
        };
        let Some(inner) = value.strip_prefix('(') else {
            continue;
        };
        let components: Vec<String> = inner[..close - 1]
            .split_whitespace()
            .map(|c| c.trim_matches('"').to_ascii_lowercase())
            .filter(|c| !c.is_empty())
            .collect();
        let mut params = SignatureParams {
            components,
            ..SignatureParams::default()
        };
        for parameter in value[close + 1..].split(';') {
            let Some((name, raw)) = parameter.split_once('=') else {
                continue;
            };
            let raw = raw.trim();
            let unquoted = raw.trim_matches('"').to_string();
            match name.trim() {
                "created" => params.created = raw.parse().ok(),
                "expires" => params.expires = raw.parse().ok(),
                "keyid" => params.key_id = Some(unquoted),
                "alg" => params.alg = Some(unquoted),
                "nonce" => params.nonce = Some(unquoted),
                "tag" => params.tag = Some(unquoted),
                _ => {}
            }
        }
        out.push((label.trim().to_string(), params));
    }
    out
}

/// Parse a `Signature` header into its labelled tags. A byte-sequence member is `:base64:`.
pub fn parse_signature(header: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for member in split_dictionary(header) {
        let Some((label, value)) = member.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let Some(encoded) = value
            .strip_prefix(':')
            .and_then(|rest| rest.strip_suffix(':'))
        else {
            continue;
        };
        let Ok(tag) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            continue;
        };
        out.push((label.trim().to_string(), tag));
    }
    out
}

/// Split a structured-field dictionary on the commas **between** members — the ones outside the
/// `(...)` of a covered-component list and outside the `:...:` of a byte sequence. A component
/// list never contains a comma, but a `tag` or `nonce` parameter may, and base64 padding sits
/// inside colons where a naive split would tear a tag in half.
fn split_dictionary(header: &str) -> Vec<&str> {
    let mut members = Vec::new();
    let mut depth = 0usize;
    let mut in_bytes = false;
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, ch) in header.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '(' if !in_quotes => depth += 1,
            ')' if !in_quotes => depth = depth.saturating_sub(1),
            ':' if !in_quotes && depth == 0 => in_bytes = !in_bytes,
            ',' if !in_quotes && !in_bytes && depth == 0 => {
                members.push(&header[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    members.push(&header[start..]);
    members
        .into_iter()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared secret of RFC 9421 §B.1.4, base64.
    const RFC_SECRET: &str =
        "uzvJfB4u3N0Jy4T7NZ75MDVcr8zSTInedJtkgcu46YW4XByzNJjxBdtjUkdJPBtbmHhIDi6pcl8jsasjlTMtDQ==";

    fn rfc_secret() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(RFC_SECRET)
            .expect("the RFC's own key")
    }

    /// The test request of RFC 9421 §B.2.
    fn rfc_request() -> NetRequest {
        NetRequest {
            method: "POST".to_string(),
            url: "https://example.com/foo?param=Value&Pet=dog".to_string(),
            headers: vec![
                ("Host".to_string(), "example.com".to_string()),
                (
                    "Date".to_string(),
                    "Tue, 20 Apr 2021 02:07:55 GMT".to_string(),
                ),
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Content-Length".to_string(), "18".to_string()),
            ],
            body: br#"{"hello": "world"}"#.to_vec(),
            timeout_ms: None,
            redirect_limit: None,
        }
    }

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

    /// **The RFC's own bytes.** §B.2.5 gives a request, a `Signature-Input`, the signature base it
    /// produces, and the resulting HMAC-SHA256 tag — so this checks canonicalization against the
    /// standard rather than against itself, which is the only check worth having for a format
    /// whose whole purpose is that two independent implementations agree.
    #[test]
    fn the_rfc_9421_hmac_sha256_vector_reproduces_exactly() {
        let params = SignatureParams {
            components: vec![
                "date".to_string(),
                "@authority".to_string(),
                "content-type".to_string(),
            ],
            created: Some(1_618_884_473),
            key_id: Some("test-shared-secret".to_string()),
            ..SignatureParams::default()
        };
        assert_eq!(
            params.to_value(),
            "(\"date\" \"@authority\" \"content-type\");created=1618884473;keyid=\"test-shared-secret\"",
            "the `Signature-Input` value of §B.2.5"
        );

        let base = signature_base(&rfc_request(), &params).expect("every component is present");
        assert_eq!(
            base,
            concat!(
                "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n",
                "\"@authority\": example.com\n",
                "\"content-type\": application/json\n",
                "\"@signature-params\": (\"date\" \"@authority\" \"content-type\");",
                "created=1618884473;keyid=\"test-shared-secret\"",
            ),
            "the signature base of §B.2.5 — no trailing newline"
        );

        let tag = SignatureAlg::HmacSha256.tag(&rfc_secret(), base.as_bytes());
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(tag),
            "pxcQw6G3AjtMBQjwo8XzkZf/bws5LelbaMk5rGIGtE8=",
            "the signature of §B.2.5"
        );
    }

    #[test]
    fn the_rfc_vector_verifies_through_the_public_door() {
        let mut req = rfc_request();
        req.headers.push((
            "Signature-Input".to_string(),
            "sig1=(\"date\" \"@authority\" \"content-type\");created=1618884473;keyid=\"test-shared-secret\"".to_string(),
        ));
        req.headers.push((
            "Signature".to_string(),
            "sig1=:pxcQw6G3AjtMBQjwo8XzkZf/bws5LelbaMk5rGIGtE8=:".to_string(),
        ));
        assert_eq!(
            verify_request(&req, &rfc_secret(), 1_618_884_473, None),
            Ok(())
        );
    }

    #[test]
    fn a_signed_request_verifies_and_a_tampered_one_does_not() {
        let key = SigningKey::new("svc", b"shared secret");
        let mut req = request("GET", "https://api.example.com/v1/orders?page=2");
        sign_request(&mut req, &key, 1_700_000_000).expect("the defaults are all derivable");
        assert_eq!(
            verify_request(&req, b"shared secret", 1_700_000_010, Some(60)),
            Ok(())
        );

        // Every covered component, altered one at a time. Each has to break the signature — a
        // component that can change without invalidating the tag is not actually covered.
        for alter in [
            (|r: &mut NetRequest| r.method = "DELETE".to_string()) as fn(&mut NetRequest),
            |r: &mut NetRequest| r.url = "https://api.example.com/v1/refunds?page=2".to_string(),
            |r: &mut NetRequest| r.url = "https://api.example.com/v1/orders?page=3".to_string(),
            |r: &mut NetRequest| r.url = "https://evil.example.net/v1/orders?page=2".to_string(),
        ] {
            let mut tampered = req.clone();
            alter(&mut tampered);
            assert_eq!(
                verify_request(&tampered, b"shared secret", 1_700_000_010, Some(60)),
                Err(VerifyFailure::Mismatch)
            );
        }
    }

    #[test]
    fn a_wrong_secret_is_a_mismatch_and_no_signature_is_absence() {
        let key = SigningKey::new("svc", b"right");
        let mut req = request("GET", "https://x.test/a");
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        assert_eq!(
            verify_request(&req, b"wrong", 1_700_000_000, None),
            Err(VerifyFailure::Mismatch)
        );
        assert_eq!(
            verify_request(&request("GET", "https://x.test/a"), b"right", 0, None),
            Err(VerifyFailure::Absent),
            "an unsigned request is not a failed signature — the caller may treat the two differently"
        );
    }

    #[test]
    fn the_freshness_window_rejects_a_replay_from_either_direction() {
        let key = SigningKey::new("svc", b"s");
        let mut req = request("GET", "https://x.test/a");
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        assert_eq!(verify_request(&req, b"s", 1_700_000_030, Some(60)), Ok(()));
        assert_eq!(
            verify_request(&req, b"s", 1_700_000_600, Some(60)),
            Err(VerifyFailure::Stale),
            "a captured request replayed ten minutes later"
        );
        assert_eq!(
            verify_request(&req, b"s", 1_699_999_000, Some(60)),
            Err(VerifyFailure::Stale),
            "and one whose `created` is in our future by more than the window"
        );
        assert_eq!(
            verify_request(&req, b"s", 1_800_000_000, None),
            Ok(()),
            "no window means no freshness check at all"
        );
    }

    #[test]
    fn stripping_a_covered_header_does_not_leave_the_signature_valid() {
        // The failure an "absent means empty" canonicalization would create: sign a header, then
        // remove it, and the base rebuilds without that line and matches.
        let key = SigningKey::new("svc", b"s")
            .with_components(vec!["@method".to_string(), "x-tenant".to_string()]);
        let mut req = request("GET", "https://x.test/a");
        req.headers
            .push(("x-tenant".to_string(), "acme".to_string()));
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        assert_eq!(verify_request(&req, b"s", 1_700_000_000, None), Ok(()));

        req.headers.retain(|(name, _)| name != "x-tenant");
        assert_eq!(
            verify_request(&req, b"s", 1_700_000_000, None),
            Err(VerifyFailure::MissingComponent)
        );
    }

    #[test]
    fn a_covered_body_digest_binds_the_payload() {
        let key = SigningKey::new("svc", b"s").with_components(vec![
            "@method".to_string(),
            "@path".to_string(),
            CONTENT_DIGEST.to_string(),
        ]);
        let mut req = request("POST", "https://x.test/charge");
        req.body = br#"{"amount":100}"#.to_vec();
        sign_request(&mut req, &key, 1_700_000_000).expect("the digest is computed, then signed");
        assert_eq!(
            header_value(&req, "content-digest"),
            Some(content_digest_value(br#"{"amount":100}"#).as_str()),
            "signing adds the header it is about to cover"
        );
        assert_eq!(verify_request(&req, b"s", 1_700_000_000, None), Ok(()));

        // Rewriting the body alone leaves the digest stale, so the covered header no longer
        // describes the payload and verification fails — which is the entire point of covering it.
        let mut tampered = req.clone();
        tampered.body = br#"{"amount":100000}"#.to_vec();
        assert_eq!(
            verify_request(&tampered, b"s", 1_700_000_000, None),
            Ok(()),
            "the digest header is what is signed, so a body change alone is invisible here…"
        );
        assert_ne!(
            content_digest_value(&tampered.body),
            content_digest_value(&req.body),
            "…which is why a receiver must recompute the digest and compare it to the header"
        );
    }

    #[test]
    fn signing_twice_leaves_one_signature() {
        // A redirected or retried request is signed again for its new target. Appending rather
        // than replacing would leave two `Signature` headers, and a receiver reading the first
        // would check a signature over a request that was never sent.
        let key = SigningKey::new("svc", b"s");
        let mut req = request("GET", "https://x.test/a");
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        req.url = "https://x.test/b".to_string();
        sign_request(&mut req, &key, 1_700_000_005).expect("signs again");
        assert_eq!(
            req.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("signature"))
                .count(),
            1
        );
        assert_eq!(verify_request(&req, b"s", 1_700_000_005, Some(60)), Ok(()));
    }

    #[test]
    fn a_repeated_header_signs_as_its_comma_joined_value() {
        // RFC 9421 §2.1. Two `x-tag` headers and one comma-joined `x-tag` are the same message, so
        // they must produce the same base — otherwise a proxy that folds headers breaks every
        // signature passing through it.
        let params = SignatureParams {
            components: vec!["x-tag".to_string()],
            ..SignatureParams::default()
        };
        let mut split = request("GET", "https://x.test/a");
        split.headers = vec![
            ("x-tag".to_string(), "  one  ".to_string()),
            ("x-tag".to_string(), "two".to_string()),
        ];
        let mut folded = request("GET", "https://x.test/a");
        folded.headers = vec![("X-Tag".to_string(), "one, two".to_string())];
        assert_eq!(
            signature_base(&split, &params).unwrap(),
            signature_base(&folded, &params).unwrap()
        );
    }

    #[test]
    fn the_derived_components_are_the_rfc_ones() {
        let mut req = request("get", "https://Example.COM:8443/foo/bar?a=1&b=2#frag");
        req.headers.clear();
        for (component, expected) in [
            ("@method", "GET"),
            ("@authority", "example.com:8443"),
            ("@scheme", "https"),
            ("@path", "/foo/bar"),
            ("@query", "?a=1&b=2"),
            ("@request-target", "/foo/bar?a=1&b=2"),
            (
                "@target-uri",
                "https://Example.COM:8443/foo/bar?a=1&b=2#frag",
            ),
        ] {
            assert_eq!(
                component_value(&req, component).as_deref(),
                Some(expected),
                "component={component}"
            );
        }
        // §2.2.7: a request with no query signs a bare `?`, so "no query" and "?" differ.
        assert_eq!(
            component_value(&request("GET", "https://x.test/a"), "@query").as_deref(),
            Some("?")
        );
        // A default port is elided, because that is what the `Host` header a server sees carries.
        assert_eq!(
            component_value(&request("GET", "https://x.test/a"), "@authority").as_deref(),
            Some("x.test")
        );
        // Components this module cannot produce for a request are absent, not empty.
        assert_eq!(component_value(&req, "@status"), None);
    }

    #[test]
    fn the_authority_a_client_signs_is_the_host_header_a_server_verifies() {
        // The interop that decides whether signing works at all: a client signs from the URL and a
        // server signs from the `Host` header, and if those two ever disagree no signature this
        // module makes verifies anywhere.
        for (url, host) in [
            ("https://api.test/x", "api.test"),
            ("https://api.test:443/x", "api.test"),
            ("http://api.test:8080/x", "api.test:8080"),
            ("http://api.test/x", "api.test"),
        ] {
            let outbound = request("GET", url);
            let mut inbound = request("GET", noeta_ext_abi::uri::path_of(url));
            inbound.headers = vec![("Host".to_string(), host.to_string())];
            assert_eq!(
                component_value(&outbound, "@authority"),
                component_value(&inbound, "@authority"),
                "url={url}"
            );
        }
    }

    #[test]
    fn a_signature_header_dictionary_survives_base64_padding_and_several_members() {
        // The naive `split(',')` bug: base64 sits inside `:...:` and a covered-component list
        // inside `(...)`, and a comma in either would tear a member in half.
        let parsed = parse_signature("sig1=:AAAA:, proxy=:BBBB:");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "sig1");
        assert_eq!(parsed[1].0, "proxy");

        let inputs = parse_signature_input(
            "sig1=(\"@method\" \"@path\");created=1;keyid=\"a\", proxy=(\"@authority\");created=2;keyid=\"b\";tag=\"x,y\"",
        );
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].1.components, vec!["@method", "@path"]);
        assert_eq!(inputs[0].1.created, Some(1));
        assert_eq!(inputs[1].1.key_id.as_deref(), Some("b"));
        assert_eq!(inputs[1].1.tag.as_deref(), Some("x,y"));
    }

    #[test]
    fn one_verifiable_signature_among_several_is_enough() {
        // A gateway adding its own signature must not invalidate the client's.
        let key = SigningKey::new("svc", b"s");
        let mut req = request("GET", "https://x.test/a");
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        let input = header_value(&req, "signature-input").unwrap().to_string();
        let signature = header_value(&req, "signature").unwrap().to_string();
        set_header(
            &mut req,
            "signature-input",
            &format!("proxy=(\"@method\");created=1700000000;keyid=\"gw\", {input}"),
        );
        set_header(
            &mut req,
            "signature",
            &format!("proxy=:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=:, {signature}"),
        );
        assert_eq!(verify_request(&req, b"s", 1_700_000_000, Some(60)), Ok(()));
    }

    #[test]
    fn an_unsupported_algorithm_is_refused_rather_than_ignored() {
        let mut req = request("GET", "https://x.test/a");
        set_header(
            &mut req,
            "signature-input",
            "sig1=(\"@method\");created=1;keyid=\"k\";alg=\"none\"",
        );
        set_header(&mut req, "signature", "sig1=:AAAA:");
        assert_eq!(
            verify_request(&req, b"s", 1, None),
            Err(VerifyFailure::UnsupportedAlg)
        );
        assert!(SignatureAlg::parse("rsa-pss-sha512").is_err());
        assert_eq!(
            SignatureAlg::parse("hmac-sha512").unwrap(),
            SignatureAlg::HmacSha512
        );
    }

    #[test]
    fn hmac_sha512_signs_and_verifies_end_to_end() {
        let key = SigningKey::new("svc", b"s").with_alg(SignatureAlg::HmacSha512);
        let mut req = request("PUT", "https://x.test/a?b=1");
        sign_request(&mut req, &key, 1_700_000_000).expect("signs");
        assert!(
            header_value(&req, "signature-input")
                .unwrap()
                .contains("alg=\"hmac-sha512\"")
        );
        assert_eq!(verify_request(&req, b"s", 1_700_000_000, Some(60)), Ok(()));
    }
}
