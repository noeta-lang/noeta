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
    fn type_name(&self) -> &'static str {
        RESPONSE_TYPE_NAME
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
        let response = host.net_fetch(self.request.clone())?;
        Ok(crate::NativeOut::Extern(crate::ExternBox::new(response)))
    }
}

// --- The server side (http-server S1) ------------------------------------------------------------

/// The registered extern-type name of an inbound HTTP request value (http-server S2): the serve
/// loop hands the handler one, and it reads the method/path/headers/body off it.
pub const REQUEST_TYPE_NAME: &str = "Request";

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
    fn type_name(&self) -> &'static str {
        REQUEST_TYPE_NAME
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
