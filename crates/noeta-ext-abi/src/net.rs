//! The `Network` capability's seam types: the request/response data that crosses
//! the [`crate::host::Network`] seam, the `Response` extern-type value behavior, and the default
//! async fetch descriptor.
//!
//! The deterministic sandbox *responder* (`sandbox_respond`, a pure function of the request) lives
//! in `noeta-stdlib` — it uses `serde_json`, which the lean ABI crate deliberately does not pull.

use crate::extern_value::ExternValue;
use std::any::Any;
use std::cmp::Ordering;

/// The registered extern-type name of an HTTP response value: `http.get(url)`
/// returns one, and it narrows (`is Response`), compares by value, and exposes accessor methods.
pub const RESPONSE_TYPE_NAME: &str = "Response";

/// `Response`'s qualified runtime identity (`{namespace}.{name}` of its `ExtType` registration in
/// `noeta-stdlib`) — what [`ExternValue::type_identity`] returns and every runtime identity
/// comparison keys on. Pre-joined so no dispatch path ever formats it.
pub const RESPONSE_TYPE_IDENTITY: &str = "std.http.Response";

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
    /// The per-request deadline in milliseconds, or `None` for the host's default.
    /// Set by a configured `Client`; the free verbs never carry one. Meaningless on an *inbound*
    /// request (the server side reuses this struct), where it stays `None`.
    pub timeout_ms: Option<u64>,
    /// How many redirects this request follows, or `None` for
    /// [`crate::redirect::DEFAULT_REDIRECT_LIMIT`]. `Some(0)` means the 3xx comes back as an
    /// ordinary response for the caller to read. Meaningless on an *inbound* request, where it
    /// stays `None`.
    ///
    /// It rides the seam rather than staying in the client because the *async* door has no
    /// synchronous caller above it: a spawned descriptor is handed a request and must decide for
    /// itself, and this is what it decides with.
    pub redirect_limit: Option<u32>,
}

/// The registered extern-type name of a transport failure.
pub const HTTP_ERROR_TYPE_NAME: &str = "HttpError";

/// `HttpError`'s qualified runtime identity — the [`RESPONSE_TYPE_IDENTITY`] twin.
pub const HTTP_ERROR_TYPE_IDENTITY: &str = "std.http.HttpError";

/// Why a request never produced a response. A *transport* failure only — an HTTP
/// error **status** is not one of these, it is an ordinary [`NetResponse`] the caller inspects with
/// `ok()`/`status()`. That split is what lets `?` mean "the network broke" and nothing else.
///
/// The classification exists so a retry policy can be written structurally rather than by matching
/// on message text: [`NetErrorKind::retryable`] is the predicate a backoff loop consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetErrorKind {
    /// The request exceeded its deadline.
    Timeout,
    /// The host name did not resolve.
    Dns,
    /// The connection could not be established, or was reset mid-flight.
    Connect,
    /// The TLS handshake or certificate validation failed.
    Tls,
    /// The response could not be read as HTTP (a malformed frame, a truncated body).
    Protocol,
    /// The server answered with a non-2xx **status**, and the caller opted into treating that as
    /// an error (`Response.error_for_status()`).
    ///
    /// Never produced by a request itself — a status is an answer, not a transport failure — so
    /// code matching on `kind()` can distinguish "the server said no" from "the response was
    /// unreadable" (`Protocol`), which sharing one variant would have made impossible.
    Status,
    /// The URL itself is not a valid request target.
    InvalidUrl,
    /// A transport failure that does not fit the classes above.
    Other,
    /// The request was abandoned because the run it belongs to is being cancelled
    /// (interruptible-io) — not a property of the network at all.
    ///
    /// Its own kind rather than [`NetErrorKind::Other`] because a program that recovers from a
    /// transport failure would otherwise treat a shutdown as a blip and try again. Retrying is
    /// exactly wrong here (see [`NetErrorKind::retryable`]), and the caller's very next safepoint
    /// ends the run regardless, so the only useful thing a `kind()` match can do with this is stop.
    Interrupted,
}

impl NetErrorKind {
    /// The surface label `HttpError.kind()` returns.
    pub fn label(self) -> &'static str {
        match self {
            NetErrorKind::Timeout => "timeout",
            NetErrorKind::Dns => "dns",
            NetErrorKind::Connect => "connect",
            NetErrorKind::Tls => "tls",
            NetErrorKind::Protocol => "protocol",
            NetErrorKind::Status => "status",
            NetErrorKind::InvalidUrl => "invalid_url",
            NetErrorKind::Other => "other",
            NetErrorKind::Interrupted => "interrupted",
        }
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// Timeouts, connect failures, and DNS misses are transient — a resolver blip, a full backlog,
    /// a rolling restart. TLS and URL failures are deterministic: the certificate will not become
    /// valid, the URL will not become parseable, so retrying only burns the budget. `Protocol` and
    /// `Other` are conservatively **not** retried, because a request that reached the server and
    /// came back mangled may well have been applied — retrying it risks a duplicate write.
    /// `Status` is not retried here either: which statuses are worth another attempt is a policy
    /// question the retry configuration answers, not a property of the kind. `Interrupted` is never
    /// retried, and that is the strongest case of all: the run is stopping, so a retry is work
    /// nobody will read.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            NetErrorKind::Timeout | NetErrorKind::Dns | NetErrorKind::Connect
        )
    }
}

/// A transport failure crossing the [`crate::host::Network`] seam: the classified
/// twin of the [`NetResponse`] success. Carries the URL so a diagnostic names the request that
/// failed without the caller having to thread it back through.
///
/// This IS the user-facing `HttpError` extern type, exactly as [`NetResponse`] is `Response`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError {
    /// What class of transport failure this is.
    pub kind: NetErrorKind,
    /// The request URL that failed.
    pub url: String,
    /// The host's rendering of the underlying failure.
    pub detail: String,
}

impl NetError {
    /// A failure of `kind` against `url`, detailed by `detail`.
    pub fn new(kind: NetErrorKind, url: impl Into<String>, detail: impl Into<String>) -> NetError {
        NetError {
            kind,
            url: url.into(),
            detail: detail.into(),
        }
    }

    /// The composed sentence the value displays as, and what `message()` returns:
    /// `timeout request to <https://api.example.com>: …` reads as one line in a log.
    pub fn message(&self) -> String {
        format!(
            "{} request to `{}`: {}",
            self.kind.label(),
            self.url,
            self.detail
        )
    }
}

/// A transport failure degrades to the pre-H6 aborting form for any path that has not yet been
/// converted to the `Result` door (and for hosts that only speak [`crate::StdError`]).
impl From<NetError> for crate::StdError {
    fn from(error: NetError) -> crate::StdError {
        crate::StdError {
            // A cancelled request keeps saying so across the seam: the aborting door must end the
            // run as *cancelled* rather than report a failed one, and this is where the kind that
            // decides that is chosen.
            kind: match error.kind {
                NetErrorKind::Interrupted => crate::ErrorKind::Interrupted,
                _ => crate::ErrorKind::Io,
            },
            message: error.message(),
        }
    }
}

/// `NetError` IS the user-facing `HttpError` extern type — pure, host-free, content-equal data,
/// the [`NetResponse`] model. Declares `Error` + `Display` at its registration so `<E: Error>`
/// bounds accept it and `?` converts through `From`.
impl ExternValue for NetError {
    fn type_identity(&self) -> &'static str {
        HTTP_ERROR_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<NetError>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.message())
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

/// An HTTP response crossing the [`crate::host::Network`] seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetResponse {
    /// The HTTP status code (e.g. `200`, `404`).
    pub status: u16,
    /// Response headers (name, value).
    pub headers: Vec<(String, String)>,
    /// The response body bytes.
    pub body: Vec<u8>,
    /// The **final** URL this response came from — after redirects, so it is the correct base for
    /// resolving a relative `Location` or RFC 8288 `Link` target. Empty for a response the program
    /// *built* rather than received (`http.server.response(…)`), which has no originating URL.
    pub url: String,
}

impl NetResponse {
    /// The value of header `name`, matched case-insensitively (HTTP header names are), or `None`.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `NetResponse` IS the user-facing `Response` extern type — pure, host-free, not
/// key-capable. Accessor methods (`status`/`ok`/`body`/`body_bytes`/`header`) dispatch through the
/// registry like `Uuid`'s; equality is by content, and it has no order.
impl ExternValue for NetResponse {
    fn type_identity(&self) -> &'static str {
        RESPONSE_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<NetResponse>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<response {}>", self.status)
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

/// Marshal a fetch outcome as the client's `Result<Response, HttpError>`. Shared by
/// the sync dispatch and every async descriptor (default, real, browser), so both doors return the
/// identical shape and `await`ing an async verb yields the same `Result` the sync verb does.
pub fn fetch_outcome(result: Result<NetResponse, NetError>) -> crate::NativeOut {
    match result {
        Ok(response) => crate::NativeOut::Ok(Box::new(crate::NativeOut::Extern(
            crate::ExternBox::new(response),
        ))),
        Err(error) => crate::NativeOut::Err(Box::new(crate::NativeOut::Extern(
            crate::ExternBox::new(error),
        ))),
    }
}

/// The default async network descriptor: it performs the request synchronously
/// through the Host **at spawn** and has no real body. The sandbox uses this (deterministic,
/// resolved at spawn — the differential never observes a real body); the real host overrides
/// [`crate::host::Network::net_spawn`] with a concurrent reqwest-backed descriptor. This is the
/// same "serial degradation for free" the fs metadata twins rely on.
#[derive(Debug)]
pub struct NetFetchIo {
    /// The request to perform when the descriptor is driven.
    pub request: NetRequest,
}

impl crate::ExternIo for NetFetchIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        // A descriptor IS the whole request, so it follows its own redirects: nothing above it is
        // synchronous enough to do so. It shares [`crate::redirect::redirect_target`] with the
        // synchronous door, so `get_async` and `get` cannot disagree about what a 302 means.
        Ok(fetch_outcome(crate::redirect::follow_redirects(
            self.request.clone(),
            |hop| host.net_fetch(hop),
        )))
    }
}

// --- The server side ------------------------------------------------------------

/// The registered extern-type name of an inbound HTTP request value: the serve
/// loop hands the handler one, and it reads the method/path/headers/body off it.
pub const REQUEST_TYPE_NAME: &str = "Request";

/// `Request`'s qualified runtime identity — the [`RESPONSE_TYPE_IDENTITY`] twin.
pub const REQUEST_TYPE_IDENTITY: &str = "std.http.Request";

/// An **inbound** HTTP request delivered to a server handler. Wraps the plain [`NetRequest`] the
/// Network seam carries plus the `conn` id the serve loop replies to — the id rides *inside* the
/// value so the loop can `net_reply` to the right connection after the handler returns, without a
/// separate connection type in the language. The handler only ever sees the request accessors, and
/// `conn` is invisible to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The connection this request arrived on — the serve loop replies here — or `None` for an
    /// **outbound** request the program built itself (`Client.prepare`), which has no connection.
    ///
    /// `Option` rather than a sentinel: connection ids start at 0, so any in-band "not a real
    /// connection" value would collide with a live socket, and a reply meant for nobody would go
    /// to a real client.
    pub conn: Option<u64>,
    /// The request line, headers, and body.
    pub inner: NetRequest,
}

/// `Request` is a pure, host-free extern type like [`NetResponse`]: accessor methods dispatch
/// through the registry, equality is by content, and it is not key-capable.
impl ExternValue for Request {
    fn type_identity(&self) -> &'static str {
        REQUEST_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Request>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(
            out,
            "<request {} {}>",
            self.inner.method,
            crate::net::request_path(&self.inner.url)
        )
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

/// Encode an accept outcome as the `Option<Request>` the async accept leaf resolves to: `Some`
/// wrapping a [`Request`] extern value, or `None` once the listener is exhausted (the serve loop
/// stops). Shared by the default (sandbox) descriptor and any real accept body.
pub fn accept_outcome(next: Option<(u64, NetRequest)>) -> crate::NativeOut {
    match next {
        Some((conn, inner)) => crate::NativeOut::Some(Box::new(crate::NativeOut::Extern(
            crate::ExternBox::new(Request {
                conn: Some(conn),
                inner,
            }),
        ))),
        None => crate::NativeOut::None,
    }
}

/// The path of `url` (between the authority and any `?`/`#`, defaulting to `/`) — a dependency-free
/// parse used for a `Request`'s `Display` and, in S2, its `path()` accessor. (The sandbox client
/// responder has its own copy in noeta-stdlib, which pulls `serde_json`; this lean one stays here.)
pub fn request_path(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let from_path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => return "/",
    };
    let end = from_path.find(['?', '#']).unwrap_or(from_path.len());
    &from_path[..end]
}

/// One hex digit's value, or `None` when the byte is not a hex digit.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode one URL component (RFC 3986): every `%XX` back to its byte, and nothing else.
///
/// The exact inverse of [`percent_encode`], and deliberately *only* that: a `+` stays a `+`. That a
/// plus means a space is the `application/x-www-form-urlencoded` rule — a property of a query
/// string or a form body, not of a URL — and applying it here would corrupt every path segment
/// containing one. [`form_decode`] is the flavor that applies it, and it is what the form and query
/// parsers below use; this is what a *path* segment and the exposed `std.http.url.decode` use.
///
/// Decoding is done over **bytes** and converted to UTF-8 once at the end, because a non-ASCII
/// character arrives as several `%XX` in a row — decoding each escape on its own would split a
/// multi-byte character into invalid fragments. A `%` that begins no valid escape is kept verbatim
/// rather than dropped, so malformed input degrades to something readable instead of vanishing.
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi * 16 + lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-**encode** one query/form value: everything outside RFC 3986's unreserved set
/// (`A-Z a-z 0-9 - _ . ~`) becomes `%XX` over its UTF-8 bytes. Space encodes as `%20` rather than
/// `+` — both decode to a space in a query string and a form body, and `%20` is the form that is
/// also correct inside a path segment. The inverse of [`percent_decode`] for any input it produces.
pub fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for &byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Percent-decode one `application/x-www-form-urlencoded` half: a `+` is a space, and everything
/// else is [`percent_decode`]'s job.
///
/// The substitution happens **before** decoding, which is what keeps an escaped plus (`%2B`) a
/// literal `+`: it is still an escape when the substitution runs, and becomes a `+` only after.
/// Doing it the other way round would turn every `%2B` into a space too.
pub fn form_decode(raw: &str) -> String {
    // Substituting on BYTES rather than `chars`: every byte but the ASCII plus is passed through
    // untouched, so a multi-byte sequence survives verbatim and is decoded by the walk below.
    let swapped: Vec<u8> = raw
        .as_bytes()
        .iter()
        .map(|&byte| if byte == b'+' { b' ' } else { byte })
        .collect();
    percent_decode(&String::from_utf8_lossy(&swapped))
}

/// Parse an `application/x-www-form-urlencoded` payload (`k=v&k2=v2`) into decoded pairs, in wire
/// order. **Both** halves are percent-decoded — a key can be encoded too — and a pair with no `=`
/// yields an empty value. Empty segments (a trailing `&`, or `&&`) are skipped rather than
/// producing a blank key. This is the one parser behind both the query string and the request body:
/// same wire format, different source.
pub fn form_pairs(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(key), form_decode(value))
        })
        .collect()
}

/// The decoded value of form field `name` in an `application/x-www-form-urlencoded` payload, or
/// `None`. First match wins, matching `query_value`/`cookie`.
pub fn form_value(body: &str, name: &str) -> Option<String> {
    form_pairs(body)
        .into_iter()
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// The value of query parameter `name` in `url`'s query string (`?k=v&k2=v2`), or `None` — the
/// dependency-free backing for a `Request`'s `query(name)` accessor. First match wins, and the
/// value is **percent-decoded**: a query string is percent-encoded by definition, so `?title=buy+milk`
/// yields `buy milk` and `?q=caf%C3%A9` yields `café`. The key is decoded before matching too.
pub fn query_value(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    form_value(query, name)
}

/// The value of request header `name`, matched case-insensitively (HTTP header names are), or
/// `None` — the inbound counterpart of [`NetResponse::header_value`].
pub fn request_header<'a>(request: &'a NetRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The default async accept descriptor: it resolves synchronously through the Host
/// at spawn (the sandbox pops its request script; any host degrades serially) and has no real body.
/// `RealHost` overrides [`crate::Network::net_accept`] with a genuine `TcpListener::accept().await`
/// future — the same "serial degradation for free" the fs metadata twins and the client's
/// [`NetFetchIo`] rely on.
#[derive(Debug)]
pub struct AcceptIo {
    /// The listener to take the next connection from.
    pub listener: u64,
}

impl crate::ExternIo for AcceptIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        Ok(accept_outcome(host.net_accept_next(self.listener)?))
    }
}

/// The default reply descriptor: writes the response through the Host at spawn
/// (the sandbox records it). `RealHost` overrides [`crate::Network::net_reply`] with an async
/// socket write. One-shot — the response is moved out on the single run.
#[derive(Debug)]
pub struct ReplyIo {
    /// The connection to reply on.
    pub conn: u64,
    /// The response to write — `Some` until the one run consumes it.
    pub response: Option<NetResponse>,
}

impl crate::ExternIo for ReplyIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let response = self
            .response
            .take()
            .expect("a reply descriptor is run exactly once");
        host.net_reply_now(self.conn, response)?;
        Ok(crate::NativeOut::Unit)
    }
}

// ------------------------------------------------------------------ websocket hijack

/// The websocket handshake GUID (RFC 6455 §4.2.2): `Sec-WebSocket-Accept` is
/// `base64(sha1(key + GUID))`. Here so both hosts (and tests) share one constant.
pub const WS_ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// One inbound websocket message crossing the hijack seam — TEXT frames only at this slice
/// (LiveView's diff-push and the HMR events are JSON text; binary is an additive variant later).
/// `None` from a recv means the peer closed (or the sandbox conversation is exhausted).
///
/// The upgrade/recv/send/close descriptor family below mirrors the accept/reply pair exactly:
/// deterministic `run_sync` through the Host (the sandbox's scripted conversation), a real host
/// overriding the builders with genuinely async bodies.
#[derive(Debug)]
pub struct WsUpgradeIo {
    /// The connection switching from one-reply-and-close to a persistent message stream.
    pub conn: u64,
    /// The client's `Sec-WebSocket-Key` — `Some` until the one run consumes it.
    pub key: Option<String>,
}

impl crate::ExternIo for WsUpgradeIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let key = self
            .key
            .take()
            .expect("an upgrade descriptor is run exactly once");
        host.net_ws_upgrade_now(self.conn, &key)?;
        Ok(crate::NativeOut::Unit)
    }
}

/// The default websocket receive descriptor: resolves synchronously through the Host (the sandbox
/// pops its scripted conversation). `RealHost` overrides [`crate::Network::net_ws_recv`] with a
/// genuine async frame read. Resolves to `?string` (`None` = closed).
#[derive(Debug)]
pub struct WsRecvIo {
    pub conn: u64,
}

impl crate::ExternIo for WsRecvIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        Ok(ws_recv_outcome(host.net_ws_recv_next(self.conn)?))
    }
}

/// The default **timed** receive descriptor. The sandbox has no real clock to wait on and a fixed
/// scripted conversation, so a deadline cannot change what arrives: it resolves exactly like
/// [`WsRecvIo`], which keeps a session that polls with a timeout deterministic and identical on
/// both backends. `RealHost` overrides [`crate::Network::net_ws_recv_timeout`] with a real wait.
#[derive(Debug)]
pub struct WsRecvTimeoutIo {
    pub conn: u64,
}

impl crate::ExternIo for WsRecvTimeoutIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        Ok(ws_recv_outcome(host.net_ws_recv_next(self.conn)?))
    }
}

/// Materialize a recv result as the language-facing `?string`.
pub fn ws_recv_outcome(next: Option<String>) -> crate::NativeOut {
    match next {
        Some(text) => crate::NativeOut::Some(Box::new(crate::NativeOut::Str(text))),
        None => crate::NativeOut::None,
    }
}

/// The default websocket send descriptor: writes through the Host at spawn (the sandbox records
/// the frame in its transcript). One-shot.
#[derive(Debug)]
pub struct WsSendIo {
    pub conn: u64,
    /// The text frame to write — `Some` until the one run consumes it.
    pub text: Option<String>,
}

impl crate::ExternIo for WsSendIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let text = self
            .text
            .take()
            .expect("a send descriptor is run exactly once");
        host.net_ws_send_now(self.conn, &text)?;
        Ok(crate::NativeOut::Unit)
    }
}

/// The default websocket close descriptor.
#[derive(Debug)]
pub struct WsCloseIo {
    pub conn: u64,
}

impl crate::ExternIo for WsCloseIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        host.net_ws_close_now(self.conn)?;
        Ok(crate::NativeOut::Unit)
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;

    #[test]
    fn percent_decode_handles_escapes_and_multibyte() {
        assert_eq!(percent_decode("a%20b"), "a b");
        // A non-ASCII character arrives as SEVERAL `%XX`; decoding per-escape would split it.
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("%F0%9F%A6%80"), "🦀");
        // Malformed escapes degrade to something readable rather than vanishing.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
        // A plus is a plus. The form rule is `form_decode`'s, because a `+` in a PATH segment is a
        // literal plus and this is the decoder a path (and `std.http.url.decode`) goes through.
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn form_decode_is_percent_decode_plus_the_one_form_rule() {
        assert_eq!(form_decode("buy+milk"), "buy milk");
        assert_eq!(form_decode("a%20b"), "a b");
        // The substitution runs BEFORE decoding, so an escaped plus survives as a literal one —
        // doing it after would turn every `%2B` into a space.
        assert_eq!(form_decode("a%2Bb"), "a+b");
        // Multi-byte input is untouched by the substitution.
        assert_eq!(form_decode("caf%C3%A9+au+lait"), "café au lait");
    }

    #[test]
    fn percent_encode_is_the_inverse_over_unreserved() {
        assert_eq!(percent_encode("buy milk"), "buy%20milk");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("café"), "caf%C3%A9");
        for raw in ["buy milk", "café", "a&b=c", "100%", "🦀"] {
            assert_eq!(percent_decode(&percent_encode(raw)), raw, "roundtrip {raw}");
        }
    }

    #[test]
    fn form_pairs_decodes_both_halves_and_skips_empties() {
        assert_eq!(
            form_pairs("title=buy+milk&done=false"),
            vec![
                ("title".to_string(), "buy milk".to_string()),
                ("done".to_string(), "false".to_string()),
            ]
        );
        // A key can be encoded too.
        assert_eq!(
            form_pairs("my%20key=v"),
            vec![("my key".to_string(), "v".to_string())]
        );
        // No `=` yields an empty value; empty segments are skipped, not blank-keyed.
        assert_eq!(
            form_pairs("flag"),
            vec![("flag".to_string(), String::new())]
        );
        assert_eq!(
            form_pairs("&a=1&&"),
            vec![("a".to_string(), "1".to_string())]
        );
        assert_eq!(form_pairs(""), vec![]);
    }

    #[test]
    fn form_value_takes_the_first_match() {
        assert_eq!(form_value("a=1&a=2", "a").as_deref(), Some("1"));
        assert_eq!(form_value("a=1", "b"), None);
    }

    #[test]
    fn query_value_percent_decodes() {
        // The regression this fixes: values arrived raw.
        assert_eq!(
            query_value("/s?title=buy+milk", "title").as_deref(),
            Some("buy milk")
        );
        assert_eq!(query_value("/s?q=caf%C3%A9", "q").as_deref(), Some("café"));
        // Fragment is not part of the query, and a missing query is None.
        assert_eq!(query_value("/s?a=1#frag", "a").as_deref(), Some("1"));
        assert_eq!(query_value("/s", "a"), None);
    }
}
