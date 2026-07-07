//! The Debug Adapter Protocol wire format: `Content-Length`-framed JSON messages over a byte stream,
//! identical framing to LSP. A DAP message is one of three shapes distinguished by `type`:
//!
//! - **request** — `{ seq, type: "request", command, arguments? }` (client → adapter)
//! - **response** — `{ seq, type: "response", request_seq, success, command, body?, message? }`
//! - **event** — `{ seq, type: "event", event, body? }`
//!
//! We keep payloads as [`serde_json::Value`] rather than typed structs for now: DAP bodies carry many
//! optional fields, and the adapter reads only a handful per request. Every outgoing message's `seq`
//! is assigned centrally by the [`Writer`] so the counter has a single owner.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

/// Read one framed message from `reader`, or `Ok(None)` at a clean end-of-stream (the client closed
/// the connection). Header lines are `Key: Value` pairs terminated by a blank line; only
/// `Content-Length` is significant. The body is exactly that many bytes of UTF-8 JSON.
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF. Clean only if it lands on a message boundary (no half-read header).
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream closed mid-header",
                ))
            };
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line: headers done
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
            })?);
        }
        // Any other header (e.g. Content-Type) is ignored, per the protocol.
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "message without Content-Length")
    })?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let value =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

/// A framed-message sink that owns the outgoing `seq` counter. All three producers (request handler,
/// the run worker, lifecycle) funnel through one `Writer` so sequence numbers stay monotonic and
/// writes never interleave — in the stdio server it lives on a dedicated thread fed by a channel.
pub struct Writer<W: Write> {
    out: W,
    seq: i64,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Writer<W> {
        Writer { out, seq: 1 }
    }

    /// Stamp `message` with the next `seq` and write it framed. `message` is a response/event object
    /// already carrying its `type` and payload; only `seq` is injected here.
    pub fn send(&mut self, mut message: Value) -> io::Result<()> {
        if let Some(obj) = message.as_object_mut() {
            obj.insert("seq".into(), json!(self.seq));
        }
        self.seq += 1;
        let body = serde_json::to_vec(&message)?;
        write!(self.out, "Content-Length: {}\r\n\r\n", body.len())?;
        self.out.write_all(&body)?;
        self.out.flush()
    }
}

/// A successful response to `request`, carrying an optional `body`.
pub fn response(request: &Value, body: Value) -> Value {
    json!({
        "type": "response",
        "request_seq": request.get("seq").cloned().unwrap_or(Value::Null),
        "success": true,
        "command": request.get("command").cloned().unwrap_or(Value::Null),
        "body": body,
    })
}

/// A failed response to `request`, with a human-readable `message`.
pub fn error_response(request: &Value, message: &str) -> Value {
    json!({
        "type": "response",
        "request_seq": request.get("seq").cloned().unwrap_or(Value::Null),
        "success": false,
        "command": request.get("command").cloned().unwrap_or(Value::Null),
        "message": message,
    })
}

/// An event named `event` with the given `body`.
pub fn event(event: &str, body: Value) -> Value {
    json!({
        "type": "event",
        "event": event,
        "body": body,
    })
}

/// The `command` field of a request, or `""` if absent/non-string.
pub fn command_of(request: &Value) -> &str {
    request.get("command").and_then(Value::as_str).unwrap_or("")
}
