//! The `wasi:http/incoming-handler` glue: convert the platform's request into the
//! neutral [`NetRequest`], run [`crate::serve_once`], and stream the [`NetResponse`] back.
//! Compiled only for the wasi target — the `wasi` crate's proxy-world bindings do not exist
//! elsewhere, and the core it wraps is natively tested without it.

use noeta_stdlib::{NetRequest, NetResponse};
use wasi::exports::http::incoming_handler::Guest;
use wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
    ResponseOutparam, StatusCode,
};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, outparam: ResponseOutparam) {
        let net_request = to_net_request(request);
        let net_response = crate::serve_once(net_request);
        respond(outparam, net_response);
    }
}

wasi::http::proxy::export!(Component);

/// Lower the platform request into the neutral shape the `Network` capability speaks.
fn to_net_request(request: IncomingRequest) -> NetRequest {
    let method = match request.method() {
        Method::Get => "GET".to_string(),
        Method::Head => "HEAD".to_string(),
        Method::Post => "POST".to_string(),
        Method::Put => "PUT".to_string(),
        Method::Delete => "DELETE".to_string(),
        Method::Connect => "CONNECT".to_string(),
        Method::Options => "OPTIONS".to_string(),
        Method::Trace => "TRACE".to_string(),
        Method::Patch => "PATCH".to_string(),
        Method::Other(other) => other,
    };
    // The path+query is what a handler routes on — the same view the bundled server's Request
    // carries (`req.path()`), so handler code is deployment-agnostic.
    let url = request.path_with_query().unwrap_or_else(|| "/".to_string());
    let headers = request
        .headers()
        .entries()
        .into_iter()
        .map(|(name, value)| (name, String::from_utf8_lossy(&value).into_owned()))
        .collect();
    let body = read_body(request);
    NetRequest {
        method,
        url,
        headers,
        body,
        // Meaningless on an inbound request (see the field docs) — the deadline is an outbound,
        // client-side concern.
        timeout_ms: None,
        redirect_limit: None,
    }
}

/// Drain the incoming body stream fully (handlers see complete bodies, like the bundled server).
fn read_body(request: IncomingRequest) -> Vec<u8> {
    let Ok(body) = request.consume() else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    {
        let Ok(stream) = body.stream() else {
            return Vec::new();
        };
        loop {
            match stream.blocking_read(64 * 1024) {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(_) => break, // closed = EOF; a transport error yields what arrived
            }
        }
    }
    IncomingBody::finish(body);
    bytes
}

/// Raise the neutral response back into platform types and hand it to the outparam.
fn respond(outparam: ResponseOutparam, response: NetResponse) {
    let fields = Fields::new();
    for (name, value) in &response.headers {
        // A header the platform refuses (forbidden name) is dropped rather than failing the
        // whole response — the body is the payload that matters.
        let _ = fields.append(name, value.as_bytes());
    }
    let outgoing = OutgoingResponse::new(fields);
    let _ = outgoing.set_status_code(response.status as StatusCode);
    let body = outgoing.body().expect("body is taken exactly once");
    ResponseOutparam::set(outparam, Ok(outgoing));
    {
        let stream = body.write().expect("stream is taken exactly once");
        let mut rest = response.body.as_slice();
        while !rest.is_empty() {
            match stream.blocking_write_and_flush(&rest[..rest.len().min(4096)]) {
                Ok(()) => rest = &rest[rest.len().min(4096)..],
                Err(_) => break, // client went away; nothing useful to do
            }
        }
    }
    let _ = OutgoingBody::finish(body, None);
}
