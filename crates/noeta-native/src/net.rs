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
