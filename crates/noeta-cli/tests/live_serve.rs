//! LiveView end-to-end over a real socket (server-hmr L2): serve the bundled counter example,
//! fetch the page and the bundled client shim over plain HTTP, then speak the view/diff protocol
//! through a real RFC 6455 websocket — snapshot on connect, JSON events in, minimal patch frames
//! out. `#[ignore]` so CI stays hermetic (real port, real process) — run explicitly:
//! `cargo test -p noeta-cli --test live_serve -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

/// One plain HTTP GET; the server closes the connection after replying, so read-to-EOF is the
/// whole response. Returns the body.
fn get(addr: &str, path: &str) -> Result<String, String> {
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
fn ws_connect(addr: &str, path: &str) -> Result<TcpStream, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
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
fn ws_send(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    assert!(payload.len() < 126, "test frames are short");
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).map_err(|e| e.to_string())
}

/// Read one server frame (unmasked); returns `(opcode, payload)`.
fn ws_recv(stream: &mut TcpStream) -> Result<(u8, String), String> {
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

#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn the_liveview_example_pushes_snapshot_and_patches_to_a_real_client() {
    let example = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/liveview_counter.noe");
    let port = 8473;
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args([
            "serve",
            example.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        let mut up = false;
        for _ in 0..80 {
            if TcpStream::connect(&addr).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !up {
            return Err("server did not accept within 4s".to_string());
        }

        // Plain HTTP: the page carries the bindings and loads the shim; the shim is served
        // source-intact from /live.js.
        let page = get(&addr, "/")?;
        for marker in [
            "data-live=\"count\"",
            "data-live-click=\"increment\"",
            "/live.js",
        ] {
            if !page.contains(marker) {
                return Err(format!("page lost `{marker}`:\n{page}"));
            }
        }
        let shim = get(&addr, "/live.js")?;
        if !shim.contains("noetaLive") {
            return Err(format!("shim not served: {shim}"));
        }

        // The protocol: snapshot on connect, then one minimal patch per event.
        let mut ws = ws_connect(&addr, "/ws")?;
        let (op, snapshot) = ws_recv(&mut ws)?;
        if op != 1 || snapshot != r#"{"type":"snapshot","values":{"count":0,"double":0}}"# {
            return Err(format!("bad snapshot: op={op} {snapshot}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"increment"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":1,"double":2}}"# {
            return Err(format!("bad first patch: {patch}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"increment"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":2,"double":4}}"# {
            return Err(format!("bad second patch: {patch}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"reset"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":0,"double":0}}"# {
            return Err(format!("bad reset patch: {patch}"));
        }

        // A second session shares the signals: its snapshot sees the (reset) current state.
        let mut ws2 = ws_connect(&addr, "/ws")?;
        let (_, snap2) = ws_recv(&mut ws2)?;
        if snap2 != r#"{"type":"snapshot","values":{"count":0,"double":0}}"# {
            return Err(format!("bad second-session snapshot: {snap2}"));
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    outcome.expect("liveview end-to-end");
}
