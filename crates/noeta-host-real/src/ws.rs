//! RFC 6455 server-side websockets on the real host: the 101 handshake, a
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
    read: Arc<tokio::sync::Mutex<ReadSide>>,
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
}

/// The read half plus the bytes read off it that have not yet been parsed into a frame.
///
/// The buffer is what makes [`RealWsRecvIo`] **cancel-safe**. A recv future can be dropped before
/// it completes — that is precisely what `std.task.race`'s loser gets, and how a session races a
/// client event against a timer. Parsing straight off the socket with a chain of `read_exact`s
/// into locals loses whatever was already consumed when the future is dropped, and the *next*
/// recv then reads the middle of a frame as a header: a silently desynchronized stream, which
/// shows up much later as a bogus opcode or a wild frame length. Every byte read lands here
/// first, and frames are parsed out of it, so a dropped future costs at most the parse — never
/// the bytes.
#[derive(Debug)]
pub(crate) struct ReadSide {
    half: OwnedReadHalf,
    buf: Vec<u8>,
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
    read: &mut ReadSide,
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

/// The largest frame accepted. A dev/LiveView frame is small; a multi-megabyte claim is a broken
/// or hostile peer.
const MAX_FRAME: u64 = 16 * 1024 * 1024;

/// Read one raw frame `(fin, opcode, unmasked payload)`; `None` on clean EOF at a frame boundary.
///
/// **Cancel-safe**: the only `.await` is [`ReadSide::fill`], which appends to `side.buf` and
/// nothing else, so dropping this future between polls loses no bytes — the next call re-parses
/// from the same buffer. Consumption is deliberately deferred to the very end (`drain`), so a
/// drop part-way through parsing a frame is not observable either.
async fn read_frame(side: &mut ReadSide) -> Result<Option<(bool, u8, Vec<u8>)>, StdError> {
    // The 2-byte prefix. EOF here — with nothing buffered — is a clean close between frames.
    while side.buf.len() < 2 {
        if !side.fill().await? {
            return if side.buf.is_empty() {
                Ok(None)
            } else {
                Err(io_error(
                    "websocket read failed mid-frame: unexpected EOF".to_string(),
                ))
            };
        }
    }
    let fin = side.buf[0] & 0x80 != 0;
    let opcode = side.buf[0] & 0x0F;
    let masked = side.buf[1] & 0x80 != 0;
    let short = (side.buf[1] & 0x7F) as u64;
    // Where the payload starts, and how long it is: the 2-byte prefix, then the extended length
    // (0/2/8 bytes), then the 4-byte mask key when present.
    let len_bytes = match short {
        126 => 2,
        127 => 8,
        _ => 0,
    };
    let mask_bytes = if masked { 4 } else { 0 };
    let header = 2 + len_bytes + mask_bytes;
    side.want(header).await?;
    let len = match short {
        126 => u16::from_be_bytes([side.buf[2], side.buf[3]]) as u64,
        127 => u64::from_be_bytes(
            side.buf[2..10]
                .try_into()
                .expect("8 bytes present — `want` filled the header"),
        ),
        n => n,
    };
    if len > MAX_FRAME {
        return Err(io_error(format!("websocket frame too large ({len} bytes)")));
    }
    let total = header + len as usize;
    side.want(total).await?;
    // Everything is buffered — from here on there is no await, so the frame is consumed atomically.
    let mut payload = side.buf[header..total].to_vec();
    if masked {
        let key = &side.buf[header - 4..header];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[i % 4];
        }
    }
    side.drain(total);
    Ok(Some((fin, opcode, payload)))
}

impl ReadSide {
    fn new(half: OwnedReadHalf) -> ReadSide {
        ReadSide {
            half,
            buf: Vec::new(),
        }
    }

    /// Read once from the socket into the buffer. `Ok(false)` on EOF.
    async fn fill(&mut self) -> Result<bool, StdError> {
        let mut chunk = [0u8; 8192];
        let n = self
            .half
            .read(&mut chunk)
            .await
            .map_err(|e| io_error(format!("websocket read failed: {e}")))?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(true)
    }

    /// Buffer at least `n` bytes. EOF before that is mid-frame, which is never clean.
    async fn want(&mut self, n: usize) -> Result<(), StdError> {
        while self.buf.len() < n {
            if !self.fill().await? {
                return Err(io_error(
                    "websocket read failed mid-frame: unexpected EOF".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Drop the first `n` bytes — one consumed frame.
    fn drain(&mut self, n: usize) {
        self.buf.drain(..n);
    }
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
                    read: Arc::new(tokio::sync::Mutex::new(ReadSide::new(read))),
                    write: Arc::new(tokio::sync::Mutex::new(write)),
                },
            );
            Ok(NativeOut::Unit)
        })))
    }
}

/// Timed receive descriptor: one message off the read half, or `none` once `ms` elapses.
///
/// The deadline wraps the read rather than racing it. That distinction is the whole point: a
/// `race(recv, timer)` cancels the losing recv, and a message it had already consumed is lost with
/// the cancelled task. Here the timeout expires *inside* the read, and because [`ReadSide`] buffers
/// every byte it pulls, expiring part-way through a frame keeps that frame's bytes for the next
/// call. Nothing is dropped either way — a partial frame or a whole message.
#[derive(Debug)]
pub(crate) struct RealWsRecvTimeoutIo {
    pub(crate) ws_conns: WsConns,
    pub(crate) conn: u64,
    pub(crate) ms: u64,
}

impl ExternIo for RealWsRecvTimeoutIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(runtime_only("websocket receive"))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let ws_conns = self.ws_conns.clone();
        let conn = self.conn;
        let ms = self.ms;
        Some(RealBody::Async(Box::pin(async move {
            let Some(ws) = ws_conns.lock().unwrap().get(&conn).cloned() else {
                return Ok(ws_recv_outcome(None));
            };
            let mut read = ws.read.lock().await;
            let waited = tokio::time::timeout(
                std::time::Duration::from_millis(ms),
                read_message(&mut read, &ws.write),
            )
            .await;
            match waited {
                // The deadline passed with no complete message. Whatever bytes arrived stay in the
                // read buffer, so the next call resumes the same frame.
                Err(_) => Ok(ws_recv_outcome(None)),
                Ok(result) => {
                    let message = result?;
                    if message.is_none() {
                        ws_conns.lock().unwrap().remove(&conn);
                    }
                    Ok(ws_recv_outcome(message))
                }
            }
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

    /// One masked client→server TEXT frame carrying `text`.
    fn masked_text_frame(text: &str, mask: [u8; 4]) -> Vec<u8> {
        let payload = text.as_bytes();
        assert!(payload.len() < 126, "test frames use the short length form");
        let mut frame = vec![0x81, 0x80 | payload.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        frame
    }

    /// Dropping a recv future part-way through a frame must not eat the bytes it had already
    /// pulled off the socket.
    ///
    /// This is the shape `std.task.race` produces — the loser's future is dropped at its next
    /// suspension — so a session racing a client event against a timer hits it on every tick that
    /// lands mid-frame. Parsing with `read_exact` into locals lost those bytes, and the *next*
    /// recv read the middle of a frame as a header: a desynchronized stream that surfaces later
    /// as a bogus opcode or a wild length, far from the cancellation that caused it.
    #[test]
    fn a_recv_dropped_mid_frame_keeps_the_bytes_it_already_read() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let client = tokio::net::TcpStream::connect(addr).await.expect("connect");
            let (server, _) = listener.accept().await.expect("accept");
            let (read, write) = server.into_split();
            let mut side = ReadSide::new(read);
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let mut client = client;

            let frame = masked_text_frame("hello world", [0xA1, 0xB2, 0xC3, 0xD4]);
            // Split inside the mask key, so a lost prefix cannot resync by luck.
            let (head, tail) = frame.split_at(5);

            client.write_all(head).await.expect("write frame head");
            // Drive the read until it is waiting for the rest, then drop it — the cancellation.
            let cancelled = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                read_message(&mut side, &write),
            )
            .await;
            assert!(cancelled.is_err(), "the partial frame must not complete");

            client.write_all(tail).await.expect("write frame tail");
            let got = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                read_message(&mut side, &write),
            )
            .await
            .expect("the completed frame arrives")
            .expect("no read error");
            assert_eq!(got.as_deref(), Some("hello world"));
        });
    }

    /// The buffer must also not *over*-consume: two frames arriving in one TCP segment both parse,
    /// in order. (The `read_exact` codec got this right by construction; a buffered one has to be
    /// held to it, since a frame is now parsed out of a buffer that may hold the next one too.)
    #[test]
    fn two_frames_in_one_segment_both_parse_in_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let client = tokio::net::TcpStream::connect(addr).await.expect("connect");
            let (server, _) = listener.accept().await.expect("accept");
            let (read, write) = server.into_split();
            let mut side = ReadSide::new(read);
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let mut client = client;

            let mut both = masked_text_frame("first", [1, 2, 3, 4]);
            both.extend(masked_text_frame("second", [9, 8, 7, 6]));
            client.write_all(&both).await.expect("write both frames");

            for want in ["first", "second"] {
                let got = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    read_message(&mut side, &write),
                )
                .await
                .expect("frame arrives")
                .expect("no read error");
                assert_eq!(got.as_deref(), Some(want));
            }
        });
    }
}
