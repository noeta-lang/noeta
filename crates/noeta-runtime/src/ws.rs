//! RFC 6455 server-side websockets on the real host (server-hmr L0b): the 101 handshake, a
//! hand-rolled TEXT-frame codec on tokio halves, and the four async descriptors overriding the
//! `Network` hijack seam's deterministic defaults.
//!
//! Scope (deliberate, matching the seam's text-only contract): text/ping/pong/close opcodes;
//! binary and fragmented (continuation) frames are refused — LiveView's diff-push and the HMR
//! client events are small JSON text frames. Client→server frames are unmasked per spec
//! (masked required, refused otherwise is *not* enforced — a missing mask bit simply reads as
//! unmasked, the lenient-reader choice); server→client frames are never masked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use noeta_stdlib::net::{WS_ACCEPT_GUID, ws_recv_outcome};
use noeta_stdlib::{ExternIo, NativeOut, RealBody, StdError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::io_error;

/// One upgraded connection: the stream's halves behind independent async locks, so a session can
/// sit in `recv().await` while `send`s proceed (and so a ping arriving mid-recv can pong).
#[derive(Debug, Clone)]
pub(crate) struct WsConn {
    read: Arc<tokio::sync::Mutex<OwnedReadHalf>>,
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
}

/// The shared upgraded-connection table on [`crate::RealHost`].
pub(crate) type WsConns = Arc<Mutex<HashMap<u64, WsConn>>>;

/// `Sec-WebSocket-Accept` for a client key: `base64(sha1(key + GUID))` (RFC 6455 §4.2.2).
pub(crate) fn accept_key(key: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_ACCEPT_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// The complete 101 handshake response for a client key.
fn handshake_response(key: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    )
}

/// Read one complete text message: transparently answers pings and swallows pongs; `None` on a
/// close frame (answered) or a clean EOF at a frame boundary.
async fn read_message(
    read: &mut OwnedReadHalf,
    write: &Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
) -> Result<Option<String>, StdError> {
    loop {
        let Some((fin, opcode, payload)) = read_frame(read).await? else {
            return Ok(None);
        };
        match opcode {
            0x1 if fin => {
                return Ok(Some(String::from_utf8_lossy(&payload).into_owned()));
            }
            // Close: answer with a close and report the stream ended.
            0x8 => {
                let mut w = write.lock().await;
                let _ = write_frame(&mut w, 0x8, &[]).await;
                return Ok(None);
            }
            // Ping → pong with the same payload; pong → ignore.
            0x9 => {
                let mut w = write.lock().await;
                write_frame(&mut w, 0xA, &payload).await?;
            }
            0xA => {}
            _ => {
                return Err(io_error(format!(
                    "unsupported websocket frame (opcode {opcode:#x}; text frames only)"
                )));
            }
        }
    }
}

/// Read one raw frame `(fin, opcode, unmasked payload)`; `None` on clean EOF at a frame boundary.
async fn read_frame(read: &mut OwnedReadHalf) -> Result<Option<(bool, u8, Vec<u8>)>, StdError> {
    let mut header = [0u8; 2];
    match read.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(io_error(format!("websocket read failed: {e}"))),
    }
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7F) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        read_all(read, &mut ext).await?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        read_all(read, &mut ext).await?;
        len = u64::from_be_bytes(ext);
    }
    // A dev/LiveView frame is small; a multi-megabyte claim is a broken or hostile peer.
    if len > 16 * 1024 * 1024 {
        return Err(io_error(format!("websocket frame too large ({len} bytes)")));
    }
    let mask = if masked {
        let mut key = [0u8; 4];
        read_all(read, &mut key).await?;
        Some(key)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    read_all(read, &mut payload).await?;
    if let Some(key) = mask {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[i % 4];
        }
    }
    Ok(Some((fin, opcode, payload)))
}

/// `read_exact` with EOF mid-frame reported as an error (EOF is only clean *between* frames).
async fn read_all(read: &mut OwnedReadHalf, buf: &mut [u8]) -> Result<(), StdError> {
    read.read_exact(buf)
        .await
        .map(|_| ())
        .map_err(|e| io_error(format!("websocket read failed mid-frame: {e}")))
}

/// Write one unmasked (server→client) frame.
async fn write_frame(
    write: &mut OwnedWriteHalf,
    opcode: u8,
    payload: &[u8],
) -> Result<(), StdError> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    write
        .write_all(&frame)
        .await
        .map_err(|e| io_error(format!("websocket write failed: {e}")))?;
    write
        .flush()
        .await
        .map_err(|e| io_error(format!("websocket flush failed: {e}")))
}

fn runtime_only(what: &str) -> StdError {
    io_error(format!("{what} requires the real executor's runtime"))
}

/// Upgrade descriptor: pull the parked stream, write the 101, split, park the halves.
#[derive(Debug)]
pub(crate) struct RealWsUpgradeIo {
    pub(crate) conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    pub(crate) ws_conns: WsConns,
    pub(crate) conn: u64,
    pub(crate) key: Option<String>,
}

impl ExternIo for RealWsUpgradeIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(runtime_only("websocket upgrade"))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let conns = self.conns.clone();
        let ws_conns = self.ws_conns.clone();
        let conn = self.conn;
        let key = self.key.take();
        Some(RealBody::Async(Box::pin(async move {
            let mut stream = conns
                .lock()
                .unwrap()
                .remove(&conn)
                .ok_or_else(|| io_error(format!("upgrade on a closed connection {conn}")))?;
            let key = key.ok_or_else(|| io_error("upgrade descriptor run twice".to_string()))?;
            stream
                .write_all(handshake_response(&key).as_bytes())
                .await
                .map_err(|e| io_error(format!("websocket handshake failed: {e}")))?;
            let (read, write) = stream.into_split();
            ws_conns.lock().unwrap().insert(
                conn,
                WsConn {
                    read: Arc::new(tokio::sync::Mutex::new(read)),
                    write: Arc::new(tokio::sync::Mutex::new(write)),
                },
            );
            Ok(NativeOut::Unit)
        })))
    }
}

/// Receive descriptor: one message off the read half (ponging pings via the write half).
#[derive(Debug)]
pub(crate) struct RealWsRecvIo {
    pub(crate) ws_conns: WsConns,
    pub(crate) conn: u64,
}

impl ExternIo for RealWsRecvIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(runtime_only("websocket receive"))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let ws_conns = self.ws_conns.clone();
        let conn = self.conn;
        Some(RealBody::Async(Box::pin(async move {
            // A missing entry means already closed: a clean `none`, not an error — the session's
            // loop shape (`recv until none`) must terminate whichever side closed first.
            let Some(ws) = ws_conns.lock().unwrap().get(&conn).cloned() else {
                return Ok(ws_recv_outcome(None));
            };
            let mut read = ws.read.lock().await;
            let message = read_message(&mut read, &ws.write).await?;
            if message.is_none() {
                ws_conns.lock().unwrap().remove(&conn);
            }
            Ok(ws_recv_outcome(message))
        })))
    }
}

/// Send descriptor: one text frame on the write half.
#[derive(Debug)]
pub(crate) struct RealWsSendIo {
    pub(crate) ws_conns: WsConns,
    pub(crate) conn: u64,
    pub(crate) text: Option<String>,
}

impl ExternIo for RealWsSendIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(runtime_only("websocket send"))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let ws_conns = self.ws_conns.clone();
        let conn = self.conn;
        let text = self.text.take();
        Some(RealBody::Async(Box::pin(async move {
            let text = text.ok_or_else(|| io_error("send descriptor run twice".to_string()))?;
            let Some(ws) = ws_conns.lock().unwrap().get(&conn).cloned() else {
                return Err(io_error(format!("send on a closed websocket {conn}")));
            };
            let mut write = ws.write.lock().await;
            write_frame(&mut write, 0x1, text.as_bytes()).await?;
            Ok(NativeOut::Unit)
        })))
    }
}

/// Close descriptor: best-effort close frame, then drop the halves.
#[derive(Debug)]
pub(crate) struct RealWsCloseIo {
    pub(crate) ws_conns: WsConns,
    pub(crate) conn: u64,
}

impl ExternIo for RealWsCloseIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(runtime_only("websocket close"))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let ws_conns = self.ws_conns.clone();
        let conn = self.conn;
        Some(RealBody::Async(Box::pin(async move {
            let removed = ws_conns.lock().unwrap().remove(&conn);
            if let Some(ws) = removed {
                let mut write = ws.write.lock().await;
                let _ = write_frame(&mut write, 0x8, &[]).await;
                let _ = write.shutdown().await;
            }
            Ok(NativeOut::Unit)
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 6455 §1.3 worked example: the spec's own key must produce the spec's own accept.
    #[test]
    fn accept_key_matches_the_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn handshake_carries_upgrade_headers_and_the_accept() {
        let resp = handshake_response("dGhlIHNhbXBsZSBub25jZQ==");
        assert!(resp.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(resp.contains("Upgrade: websocket\r\n"));
        assert!(resp.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(resp.ends_with("\r\n\r\n"));
    }
}
