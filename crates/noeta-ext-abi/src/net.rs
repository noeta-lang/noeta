//! The `Network` capability's seam types (http arc H1): the request/response data that crosses
//! the [`crate::host::Network`] seam, the `Response` extern-type value behavior, and the default
//! async fetch descriptor.
//!
//! The deterministic sandbox *responder* (`sandbox_respond`, a pure function of the request) lives
//! in `noeta-stdlib` — it uses `serde_json`, which the lean ABI crate deliberately does not pull.

use crate::extern_value::ExternValue;
use std::any::Any;
use std::cmp::Ordering;

/// The registered extern-type name of an HTTP response value (http arc H2): `http.get(url)`
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
    /// The per-request deadline in milliseconds (http arc H7), or `None` for the host's default.
    /// Set by a configured `Client`; the free verbs never carry one. Meaningless on an *inbound*
    /// request (the server side reuses this struct), where it stays `None`.
    pub timeout_ms: Option<u64>,
}

/// The registered extern-type name of a transport failure (http arc H6).
pub const HTTP_ERROR_TYPE_NAME: &str = "HttpError";

/// `HttpError`'s qualified runtime identity — the [`RESPONSE_TYPE_IDENTITY`] twin.
pub const HTTP_ERROR_TYPE_IDENTITY: &str = "std.http.HttpError";

/// Why a request never produced a response (http arc H6). A *transport* failure only — an HTTP
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
    /// The URL itself is not a valid request target.
    InvalidUrl,
    /// A transport failure that does not fit the classes above.
    Other,
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
            NetErrorKind::InvalidUrl => "invalid_url",
            NetErrorKind::Other => "other",
        }
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// Timeouts, connect failures, and DNS misses are transient — a resolver blip, a full backlog,
    /// a rolling restart. TLS and URL failures are deterministic: the certificate will not become
    /// valid, the URL will not become parseable, so retrying only burns the budget. `Protocol` and
    /// `Other` are conservatively **not** retried, because a request that reached the server and
    /// came back mangled may well have been applied — retrying it risks a duplicate write.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            NetErrorKind::Timeout | NetErrorKind::Dns | NetErrorKind::Connect
        )
    }
}

/// A transport failure crossing the [`crate::host::Network`] seam (http arc H6): the classified
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
    /// `timeout request to `https://api.example.com`: …` reads as one line in a log.
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
            kind: crate::ErrorKind::Io,
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

/// `NetResponse` IS the user-facing `Response` extern type (http arc H2) — pure, host-free, not
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

/// Marshal a fetch outcome as the client's `Result<Response, HttpError>` (http arc H6). Shared by
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

/// The default async network descriptor (http arc H3): it performs the request synchronously
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
        Ok(fetch_outcome(host.net_fetch(self.request.clone())))
    }
}

// --- The server side (http-server S1) ------------------------------------------------------------

/// The registered extern-type name of an inbound HTTP request value (http-server S2): the serve
/// loop hands the handler one, and it reads the method/path/headers/body off it.
pub const REQUEST_TYPE_NAME: &str = "Request";

/// `Request`'s qualified runtime identity — the [`RESPONSE_TYPE_IDENTITY`] twin.
pub const REQUEST_TYPE_IDENTITY: &str = "std.http.Request";

/// An **inbound** HTTP request delivered to a server handler. Wraps the plain [`NetRequest`] the
/// Network seam carries plus the `conn` id the serve loop replies to — the id rides *inside* the
/// value so the loop can `net_reply` to the right connection after the handler returns, without a
/// separate connection type in the language. The handler only ever sees the request accessors
/// (http-server S2); `conn` is invisible to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The connection this request arrived on — the serve loop replies here.
    pub conn: u64,
    /// The request line, headers, and body.
    pub inner: NetRequest,
}

/// `Request` is a pure, host-free extern type like [`NetResponse`]: accessor methods (S2) dispatch
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
            crate::ExternBox::new(Request { conn, inner }),
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

/// The value of query parameter `name` in `url`'s query string (`?k=v&k2=v2`), or `None` — the
/// dependency-free backing for a `Request`'s `query(name)` accessor (S2). First match wins;
/// percent-decoding is a follow-on (values arrive raw), matching the minimal parse the sandbox
/// uses elsewhere.
pub fn query_value(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| value.to_string())
    })
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

/// The default async accept descriptor (http-server S1): it resolves synchronously through the Host
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

/// The default reply descriptor (http-server S1): writes the response through the Host at spawn
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

// ------------------------------------------------------------------ websocket hijack (L0)

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
