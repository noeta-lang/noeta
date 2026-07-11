//! The `wasi:http/outgoing-handler` glue (P-WASM W4 follow-up): the platform's HTTP client,
//! injected into [`noeta_wasi_host::WasiHost`] as its outbound hook — so an edge handler's
//! `client.get(...)` reaches upstream services through the host (connection-pooled, no CORS),
//! closing the Network capability's last honest-error on the serve path.
//!
//! The synchronous shape comes from wasip2's own primitives: `handle()` returns a
//! `FutureIncomingResponse`, and blocking on its **pollable** (`subscribe().block()`) is the
//! platform-native way to wait — the mirror of the JSPI pump's `js_wait`, one level down.
//! Compiled only for the wasi target; natively the hook seam is exercised with a canned closure
//! (see the lib tests).

use noeta_stdlib::{ErrorKind, NetRequest, NetResponse, StdError};
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};

fn io_error(message: String) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message,
    }
}

/// Perform `request` through the platform's outgoing handler.
pub fn fetch(request: NetRequest) -> Result<NetResponse, StdError> {
    let url = request.url.clone();
    let fail = |what: &str| io_error(format!("cannot fetch `{url}`: {what}"));

    // Split the URL into the component model's scheme/authority/path-with-query triple.
    let (scheme, rest) = request
        .url
        .split_once("://")
        .ok_or_else(|| fail("the URL has no scheme"))?;
    let (authority, path_with_query) = match rest.split_once('/') {
        Some((authority, path)) => (authority.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let scheme = match scheme {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        other => Scheme::Other(other.to_string()),
    };

    let headers = Fields::new();
    for (name, value) in &request.headers {
        headers
            .append(name, value.as_bytes())
            .map_err(|e| fail(&format!("header `{name}` refused: {e:?}")))?;
    }
    let outgoing = OutgoingRequest::new(headers);
    let method = match request.method.as_str() {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "CONNECT" => Method::Connect,
        "OPTIONS" => Method::Options,
        "TRACE" => Method::Trace,
        "PATCH" => Method::Patch,
        other => Method::Other(other.to_string()),
    };
    outgoing
        .set_method(&method)
        .map_err(|()| fail("the method was refused"))?;
    outgoing
        .set_scheme(Some(&scheme))
        .map_err(|()| fail("the scheme was refused"))?;
    outgoing
        .set_authority(Some(&authority))
        .map_err(|()| fail("the authority was refused"))?;
    outgoing
        .set_path_with_query(Some(&path_with_query))
        .map_err(|()| fail("the path was refused"))?;

    // Write the request body (if any) before handing the request off.
    let body = outgoing.body().expect("body is taken exactly once");
    if !request.body.is_empty() {
        let stream = body.write().expect("stream is taken exactly once");
        let mut rest = request.body.as_slice();
        while !rest.is_empty() {
            let chunk = &rest[..rest.len().min(4096)];
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|e| fail(&format!("writing the request body failed: {e:?}")))?;
            rest = &rest[chunk.len()..];
        }
        drop(stream);
    }
    OutgoingBody::finish(body, None).map_err(|e| fail(&format!("finishing the body: {e:?}")))?;

    // Hand off and block on the response future's pollable — wasip2's native "wait here".
    let future = outgoing_handler::handle(outgoing, None)
        .map_err(|e| fail(&format!("the handler refused the request: {e:?}")))?;
    let response = loop {
        match future.get() {
            Some(outcome) => {
                break outcome
                    .map_err(|()| fail("the response future was consumed twice"))?
                    .map_err(|e| fail(&format!("transport error: {e:?}")))?;
            }
            None => future.subscribe().block(),
        }
    };

    let status = response.status();
    let headers = response
        .headers()
        .entries()
        .into_iter()
        .map(|(name, value)| (name, String::from_utf8_lossy(&value).into_owned()))
        .collect();
    // Drain the response body fully — handlers see complete bodies, like the inbound side.
    let incoming = response.consume().expect("body is consumed exactly once");
    let mut bytes = Vec::new();
    {
        let stream = incoming.stream().expect("stream is taken exactly once");
        loop {
            match stream.blocking_read(64 * 1024) {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => bytes.extend_from_slice(&chunk),
                Err(_) => break, // closed = EOF; a transport error yields what arrived
            }
        }
    }
    wasi::http::types::IncomingBody::finish(incoming);

    Ok(NetResponse {
        status,
        headers,
        body: bytes,
    })
}
