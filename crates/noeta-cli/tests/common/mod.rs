//! Shared helpers for the real-socket integration tests (`live_serve`, `hot_live`): a plain
//! HTTP GET and a minimal RFC 6455 **client** (handshake with the RFC §1.3 example key, masked
//! client frames, unmasked server frames) — just enough to speak to `server.websocket`.
#![allow(dead_code)] // each test target compiles this module; not every target uses every helper

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// One plain HTTP GET; the server closes the connection after replying, so read-to-EOF is the
/// whole response. Returns the body.
pub fn get(addr: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| e.to_string())?;
    resp.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or_else(|| "no body".to_string())
}

/// Open a websocket: HTTP upgrade with the RFC §1.3 example key, asserting the pinned accept.
pub fn ws_connect(addr: &str, path: &str) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    // Generous on purpose. These tests assert WHAT the server pushes, never how fast — but a read
    // timeout surfaces as `Resource temporarily unavailable`, which reads exactly like a missing
    // frame and has repeatedly sent people diagnosing a real bug. A developer machine building
    // another crate (or a loaded CI runner) can push a swap past a few seconds without anything
    // being wrong, so the bound is here only to stop a hung test from hanging forever.
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    // Read the 101 response up to the blank line (byte-at-a-time is fine for a handshake).
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).map_err(|e| e.to_string())?;
        head.push(byte[0]);
        if head.len() > 4096 {
            return Err("oversized handshake response".to_string());
        }
    }
    let head = String::from_utf8_lossy(&head);
    if !head.starts_with("HTTP/1.1 101") {
        return Err(format!("expected 101, got: {head}"));
    }
    if !head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=") {
        return Err(format!("wrong accept key in: {head}"));
    }
    Ok(stream)
}

/// Send one masked client text frame (RFC 6455 §5.2; clients MUST mask).
pub fn ws_send(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    assert!(payload.len() < 126, "test frames are short");
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).map_err(|e| e.to_string())
}

/// Read one server frame (unmasked); returns `(opcode, payload)`.
pub fn ws_recv(stream: &mut TcpStream) -> Result<(u8, String), String> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).map_err(|e| e.to_string())?;
    let opcode = head[0] & 0x0f;
    let mut len = (head[1] & 0x7f) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        stream.read_exact(&mut ext).map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(ext) as usize;
    }
    assert!(
        head[1] & 0x80 == 0,
        "server frames must not be masked (RFC 6455 §5.1)"
    );
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    Ok((opcode, String::from_utf8_lossy(&payload).into_owned()))
}
