//! Streaming HTTP bodies on the real host: a genuinely incremental reqwest
//! body reader, and a `text/event-stream` writer over the served connection.
//!
//! The **read** side has one structural constraint worth stating, because it dictates the design.
//! A reqwest response whose body is still arriving is tied to the runtime that drove `send()`:
//! hyper's connection task lives there, and nothing else will poll it. `RealHost`'s own runtime is
//! `current_thread` and only runs inside `block_on`, so a body opened on it and then read from the
//! *executor's* runtime would simply stall — no error, no data, a hung stream. And the `Network`
//! seam gives a host no access to the executor's runtime, so the body cannot be opened there
//! either.
//!
//! So each stream gets **its own thread with its own runtime**, which owns the request from
//! `send()` through the last byte. The head is handed back over a channel so `stream(...)` can
//! still fail as a `NetError` (the whole point of opening being synchronous), and frames flow back
//! over a **bounded** channel — bounded so a fast producer against a slow reader applies
//! backpressure instead of buffering a whole model response in memory. One thread per open stream
//! is the cost; a stream is long-lived and few, which is the case that pays for it.
//!
//! **And it gets its own `reqwest::Client`, which is the other half of the same constraint.** A
//! client is a connection *pool*, and a pooled connection carries its hyper driver task with it —
//! so handing this thread the host's client hands it connections belonging to the host's runtime,
//! and the rule above is broken by reuse rather than by construction. That is not hypothetical: a
//! program that made one ordinary `client.get(...)` and then opened a stream **to the same origin**
//! hung forever with nothing on the wire. reqwest handed the pump the idle connection from the
//! first call, whose driver lives on the host's `current_thread` runtime; that runtime is only
//! driven inside `net_fetch`'s `block_on`, so nobody polled it, the request bytes were never
//! written, and `send()` never resolved — no error, no timeout, no data. The one-shot request came
//! first, so the *stream* looked like the broken half. Measured on the socket: 62 bytes out, 984
//! in, both from the earlier `GET`, and nothing since.
//!
//! A per-stream client costs a TLS root store per open. A stream is long-lived and few (the same
//! trade the thread already makes), and no amount of pooling is worth a hang that presents as a
//! silent hang in someone else's code.
//!
//! Dropping the receiving end (an explicit `close()`, or host teardown) makes the pump's next send
//! fail, so the thread exits and the connection is released — real reclamation, not a leak waiting
//! for the process to end.
//!
//! The **write** side is ordinary HTTP/1.1 with **chunked** framing. Chunked rather than
//! close-delimited because each frame must be independently flushable — that *is* server-sent
//! events — and chunked is the framing that lets the connection stay open while it happens.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use noeta_stdlib::{ExternIo, NativeOut, RealBody, StdError};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::io_error;

// The read half is reqwest-backed, so it rides the client ring (DCE Axis B): a binary that never
// imports `std.http.client` links neither reqwest nor this. The WRITE half below is plain tokio
// over the served socket and stays ungated, exactly like the rest of the server side.
#[cfg(feature = "ring-http-client")]
pub(crate) use read::{RealStreamRecvIo, RealStreams, open};

#[cfg(feature = "ring-http-client")]
mod read {
    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::sync::{Arc, Mutex};

    use noeta_stdlib::stream::{Frame, FrameDecoder, Framing, Utf8Chunker, frame_recv_outcome};
    use noeta_stdlib::{
        ExternIo, NativeOut, NetError, NetErrorKind, NetRequest, RealBody, StdError,
    };

    use crate::io_error;

    /// How many decoded frames may sit between the pump and the reader before the pump blocks.
    ///
    /// Bounded on purpose. An unbounded channel turns "the server is faster than the program" into
    /// unbounded memory — for a token stream from a large model, the entire response — and hides it
    /// until an allocation fails. A small bound makes the socket apply the backpressure instead, which
    /// is what TCP is for. Large enough that an ordinary reader never touches the bound.
    const FRAME_BUFFER: usize = 64;

    /// One open incremental body: the receiving end of its pump thread's frame channel.
    ///
    /// Behind a `Mutex` because a recv descriptor runs on the executor's blocking pool, off the host's
    /// thread. Dropping this is what tears the stream down.
    ///
    /// The channel carries [`Pumped`] rather than a bare `Frame`, and the extra variants are the
    /// whole reason: a reader parked on `recv` is roused by *something arriving*, and a
    /// `std::sync::mpsc` receiver has no other door. So the interruption is a message the
    /// cancellation hook pushes (see [`StreamWaker`]) — which, unlike a timeout loop, wakes a quiet
    /// stream at the moment it is cancelled rather than at the end of the next tick.
    #[derive(Debug)]
    pub(crate) struct RealStream {
        frames: Arc<Mutex<Receiver<Pumped>>>,
        /// Whether the body has ended, latched by whichever read saw [`Pumped::Ended`]. Sticky
        /// because the end arrives **once**: holding a sender for the interruption keeps the
        /// channel open, so a second read after the end would have nothing to disconnect from and
        /// would wait for a pump that has already gone.
        ended: Arc<std::sync::atomic::AtomicBool>,
        /// The interruption end of the same channel, held so it lives exactly as long as the
        /// stream: the host registers it weakly, so closing the stream also drops it from the
        /// cancellation fan-out.
        waker: Arc<StreamWaker>,
    }

    impl RealStream {
        /// This stream's interruption end, for the host to register on the run's cancellation.
        pub(crate) fn waker(&self) -> &Arc<StreamWaker> {
            &self.waker
        }
    }

    /// What travels down a stream's frame channel.
    #[derive(Debug)]
    pub(crate) enum Pumped {
        /// One decoded frame of the body.
        Frame(Frame),
        /// The body ended — sent **once**, by the pump thread as it exits, for any reason it
        /// exits. Explicit rather than inferred from the channel disconnecting, because the
        /// interruption end below holds a sender of its own and a channel with a live sender never
        /// disconnects.
        Ended,
        /// The run this stream belongs to is being cancelled. Only rouses; the reader re-reads the
        /// flag and decides, so a message that outlived its (already honored) request is ignored.
        Interrupted,
    }

    /// The cancellation party for one open stream: a sender it pushes an end-my-wait message down.
    #[derive(Debug)]
    pub(crate) struct StreamWaker(SyncSender<Pumped>);

    impl crate::cancel::CancelParty for StreamWaker {
        fn interrupt(&self) {
            // `try_send`, because this must not block and a full buffer means the reader is not
            // waiting anyway — it will read the flag before its next wait. A closed channel means
            // the pump is gone, which ends the wait by itself.
            let _ = self.0.try_send(Pumped::Interrupted);
        }
    }

    /// The shared open-stream table on [`crate::RealHost`].
    pub(crate) type RealStreams = Arc<Mutex<HashMap<u64, RealStream>>>;

    /// The response head the pump reports back over the head channel — the status/headers/url half
    /// of [`noeta_stdlib::stream::StreamHead`], before the host has an id to pair it with.
    ///
    /// Reported rather than discarded because it is the *only* moment the status exists on this
    /// path: the body that follows is frames, and a vendor's non-2xx body is not an event stream at
    /// all, so an SSE reader that never saw the head sees an empty stream and nothing else.
    pub(crate) struct ResponseHead {
        pub(crate) status: u16,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) url: String,
    }

    /// Read the head out of a live reqwest response: the status, every header (repeats included, in
    /// wire order), and the **final** URL after any redirects.
    fn response_head(response: &reqwest::Response) -> ResponseHead {
        ResponseHead {
            status: response.status().as_u16(),
            headers: response
                .headers()
                .iter()
                // A header whose value is not valid UTF-8 is dropped rather than lossily mangled:
                // the seam's vocabulary is `String`, and a mangled value reads as real data.
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_string(), v.to_string()))
                })
                .collect(),
            url: response.url().to_string(),
        }
    }

    /// Open `request`'s body as a frame stream, blocking only until the response **head** is in.
    ///
    /// Returns the receiving half **and the head** on success. A transport failure surfaces here as a
    /// [`NetError`], the same door the one-shot verbs use; once the head is in, a body failure just
    /// ends the stream — there is no longer a `Result` to carry it, and a partially-consumed stream
    /// that ends early is exactly what a truncated body looks like to a reader. A non-2xx **status**
    /// is not a failure at either point: it opens successfully and the caller reads `status()`.
    pub(crate) fn open(
        request: NetRequest,
        framing: Framing,
    ) -> Result<(RealStream, ResponseHead), NetError> {
        let (head_tx, head_rx) = sync_channel::<Result<ResponseHead, NetError>>(1);
        let (frame_tx, frame_rx) = sync_channel::<Pumped>(FRAME_BUFFER);
        // The interruption end, cloned before the pump takes ownership of the sending half.
        let waker = Arc::new(StreamWaker(frame_tx.clone()));
        let url = request.url.clone();

        std::thread::Builder::new()
            .name("noeta-http-stream".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        // The head channel is the only way to report this; if the opener has already
                        // given up there is nothing to tell.
                        let _ = head_tx.send(Err(NetError::new(
                            NetErrorKind::Other,
                            &request.url,
                            format!("cannot start a runtime for the stream: {e}"),
                        )));
                        return;
                    }
                };
                // Built INSIDE the runtime context (see the module header): every connection this
                // client opens is then driven by the runtime that is about to block on it, and no
                // connection from any other runtime's pool can be handed to it.
                let _enter = runtime.enter();
                let client = reqwest::Client::new();
                drop(_enter);
                runtime.block_on(pump(client, request, framing, head_tx, frame_tx.clone()));
                // The end of the body, from the **one** place that knows the pump is done — every
                // way out of `pump` passes through here, including the ones that never sent a
                // frame at all.
                let _ = frame_tx.send(Pumped::Ended);
            })
            .map_err(|e| {
                NetError::new(
                    NetErrorKind::Other,
                    &url,
                    format!("cannot start the stream reader: {e}"),
                )
            })?;

        // The pump always reports the head exactly once; a disconnect here means the thread died
        // before it could, which is an internal failure rather than a transport one.
        match head_rx.recv() {
            Ok(Ok(head)) => Ok((
                RealStream {
                    frames: Arc::new(Mutex::new(frame_rx)),
                    ended: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    waker,
                },
                head,
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(NetError::new(
                NetErrorKind::Other,
                &url,
                "the stream reader stopped before the response head arrived",
            )),
        }
    }

    /// The body pump: send the request, report the head, then decode chunks into frames until the body
    /// ends or the reader goes away.
    async fn pump(
        client: reqwest::Client,
        request: NetRequest,
        framing: Framing,
        head_tx: SyncSender<Result<ResponseHead, NetError>>,
        frame_tx: SyncSender<Pumped>,
    ) {
        let url = request.url.clone();
        let response = match send_head(&client, request).await {
            Ok(response) => {
                if head_tx.send(Ok(response_head(&response))).is_err() {
                    return; // the opener gave up
                }
                response
            }
            Err(error) => {
                let _ = head_tx.send(Err(error));
                return;
            }
        };

        let mut decoder = FrameDecoder::new(framing);
        let mut chunker = Utf8Chunker::new();
        let mut response = response;
        loop {
            match response.chunk().await {
                Ok(Some(bytes)) => decoder.feed_str(&chunker.push(&bytes)),
                // End of body: settle the tail, which is where a line framing emits its final
                // unterminated line and SSE discards an unterminated block.
                Ok(None) => break,
                // A body-read failure (a reset mid-stream, a decode error) ENDS the stream rather than
                // reporting: the caller's `Result` door closed when the head arrived, and a reader
                // that has already consumed frames needs the loop to terminate, not an abort. The
                // truncated-body semantics below then apply, which is the honest reading of a body
                // that stopped early.
                Err(_) => break,
            }
            if !forward(&mut decoder, &frame_tx) {
                return; // the reader closed the stream
            }
        }
        decoder.feed_str(&chunker.finish());
        decoder.finish();
        let _ = url; // retained above only for the head-error path
        forward(&mut decoder, &frame_tx);
    }

    /// Send the request and return the streaming response, mapping a transport failure onto the seam's
    /// classified error. The head-only half of [`crate::reqwest_fetch`].
    async fn send_head(
        client: &reqwest::Client,
        request: NetRequest,
    ) -> Result<reqwest::Response, NetError> {
        let url = request.url.clone();
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            NetError::new(
                NetErrorKind::InvalidUrl,
                &url,
                format!("invalid HTTP method `{}`", request.method),
            )
        })?;
        let mut builder = client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body);
        }
        // A configured client's deadline bounds **establishing** the stream, and nothing after it.
        //
        // Deliberately not `RequestBuilder::timeout`, which the one-shot path uses: that covers the
        // body read too, so on a stream it would abort every healthy connection the moment the
        // deadline passed — a 30s timeout would cap a token stream at 30s of output. Wrapping only
        // `send()` (which resolves when the response head arrives) gives the deadline the meaning a
        // caller expects from a stream: how long to wait for the server to start answering.
        let send = builder.send();
        let response = match request.timeout_ms {
            Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), send)
                .await
                .map_err(|_| {
                    NetError::new(
                        NetErrorKind::Timeout,
                        &url,
                        format!("the response head did not arrive within {ms}ms"),
                    )
                })?,
            None => send.await,
        };
        response.map_err(|e| crate::net_error(&url, &e))
    }

    /// Push every ready frame to the reader. Returns `false` once the reader has gone away, which is
    /// the signal to tear the connection down.
    fn forward(decoder: &mut FrameDecoder, frame_tx: &SyncSender<Pumped>) -> bool {
        while let Some(frame) = decoder.next_frame() {
            // A blocking send: this IS the backpressure — a slow reader stops the pump, which stops
            // reading the socket, which lets TCP slow the server down.
            if frame_tx.send(Pumped::Frame(frame)).is_err() {
                return false;
            }
        }
        true
    }

    /// The real incremental-read descriptor. Its body blocks on the frame channel, so it runs on the
    /// executor's **blocking pool** — genuinely concurrent with the isolate's other tasks, and the
    /// natural fit for a channel receive.
    #[derive(Debug)]
    pub(crate) struct RealStreamRecvIo {
        pub(crate) streams: RealStreams,
        pub(crate) stream: u64,
        /// The run's cancellation token, so a read of a stream that has gone quiet — an idle SSE
        /// connection is the ordinary case, not the pathological one — ends when the run does.
        pub(crate) cancel: Arc<crate::cancel::HostCancel>,
    }

    impl ExternIo for RealStreamRecvIo {
        fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
            Err(io_error(
                "an incremental body read requires the real executor's runtime".to_string(),
            ))
        }

        fn run_real(&mut self) -> Option<RealBody> {
            let open = self
                .streams
                .lock()
                .unwrap()
                .get(&self.stream)
                .map(|s| (s.frames.clone(), Arc::clone(&s.ended)));
            let cancel = Arc::clone(&self.cancel);
            Some(RealBody::Blocking(Box::new(move || {
                // A stream that is not open reads as ended, matching the sandbox: after `close()` the
                // honest answer to "is there more?" is no. A stream whose body already ended reads
                // the same way, without waiting on a pump that has gone.
                let Some((frames, ended)) = open else {
                    return Ok(frame_recv_outcome(None));
                };
                if ended.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(frame_recv_outcome(None));
                }
                // A cancellation that was requested *before* this read started: the host's hook has
                // already fired, so no message is coming to rouse us and the flag is the only thing
                // that can answer.
                if cancel.pending() {
                    return Err(crate::interrupted_here("recv"));
                }
                let frames = frames.lock().unwrap();
                loop {
                    match frames.recv() {
                        Ok(Pumped::Frame(frame)) => return Ok(frame_recv_outcome(Some(frame))),
                        // The pump is done; latch it so a later read answers without waiting.
                        // A disconnect means the pump thread died before it could say so, which
                        // reads the same way — a body that stopped early is a truncated body.
                        Ok(Pumped::Ended) | Err(_) => {
                            ended.store(true, std::sync::atomic::Ordering::Relaxed);
                            return Ok(frame_recv_outcome(None));
                        }
                        // Roused. The flag decides: a request that has since been honored leaves
                        // this message behind, and waiting on is the right answer to it.
                        Ok(Pumped::Interrupted) => {
                            if cancel.pending() {
                                return Err(crate::interrupted_here("recv"));
                            }
                        }
                    }
                }
            })))
        }
    }
} // mod read

// ----------------------------------------------------------------------- the server (write) side

/// The shared event-stream connection table on [`crate::RealHost`]: a connection lives here from
/// `sse` start until close, having moved out of the ordinary reply table.
pub(crate) type SseConns = Arc<Mutex<HashMap<u64, TcpStream>>>;

/// The `text/event-stream` response head. **Chunked**, so each frame is independently flushable
/// while the connection stays open — close-delimited framing would work for a browser but gives an
/// intermediary every excuse to buffer the stream, which defeats the point. `x-accel-buffering`
/// turns off nginx's proxy buffer for the same reason.
fn stream_head() -> String {
    let mut head = String::from("HTTP/1.1 200 OK\r\n");
    for (name, value) in noeta_stdlib::stream::SSE_HEADERS {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("transfer-encoding: chunked\r\n");
    head.push_str("x-accel-buffering: no\r\n\r\n");
    head
}

/// One HTTP/1.1 chunk carrying `payload`.
fn chunk(payload: &str) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

/// Begin an event stream: move the connection out of the reply table and write the head.
#[derive(Debug)]
pub(crate) struct RealSseStartIo {
    pub(crate) conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    pub(crate) sse_conns: SseConns,
    pub(crate) conn: u64,
}

impl ExternIo for RealSseStartIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(io_error(
            "starting an event stream requires the real executor's runtime".to_string(),
        ))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let conns = self.conns.clone();
        let sse_conns = self.sse_conns.clone();
        let conn = self.conn;
        Some(RealBody::Async(Box::pin(async move {
            let mut stream =
                conns.lock().unwrap().remove(&conn).ok_or_else(|| {
                    io_error(format!("event stream on a closed connection {conn}"))
                })?;
            stream
                .write_all(stream_head().as_bytes())
                .await
                .map_err(|e| io_error(format!("writing the event-stream head failed: {e}")))?;
            stream
                .flush()
                .await
                .map_err(|e| io_error(format!("flushing the event-stream head failed: {e}")))?;
            sse_conns.lock().unwrap().insert(conn, stream);
            Ok(NativeOut::Unit)
        })))
    }
}

/// Write one frame's (or comment's) already-encoded bytes and flush immediately.
#[derive(Debug)]
pub(crate) struct RealSseSendIo {
    pub(crate) sse_conns: SseConns,
    pub(crate) conn: u64,
    pub(crate) wire: Option<String>,
}

impl ExternIo for RealSseSendIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(io_error(
            "an event-stream write requires the real executor's runtime".to_string(),
        ))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let sse_conns = self.sse_conns.clone();
        let conn = self.conn;
        let wire = self.wire.take();
        Some(RealBody::Async(Box::pin(async move {
            let wire = wire.ok_or_else(|| io_error("send descriptor run twice".to_string()))?;
            // Held out of the table for the duration of the write, so two concurrent sends on one
            // connection cannot interleave halves of two chunks into a corrupt stream.
            let taken = sse_conns.lock().unwrap().remove(&conn);
            let Some(mut stream) = taken else {
                // The peer went away (or the handler already closed). Dropping the frame is right:
                // a disconnected client is ordinary for an event stream, not a program error.
                return Ok(NativeOut::Unit);
            };
            let outcome = async {
                stream.write_all(&chunk(&wire)).await?;
                // Flushing per frame is the entire point: a buffered event stream is a slow poll.
                stream.flush().await
            }
            .await;
            match outcome {
                Ok(()) => {
                    sse_conns.lock().unwrap().insert(conn, stream);
                    Ok(NativeOut::Unit)
                }
                // A write failure means the client is gone; drop the connection and carry on.
                Err(_) => Ok(NativeOut::Unit),
            }
        })))
    }
}

/// End an event stream: write the terminating zero-length chunk and close.
#[derive(Debug)]
pub(crate) struct RealSseCloseIo {
    pub(crate) sse_conns: SseConns,
    pub(crate) conn: u64,
}

impl ExternIo for RealSseCloseIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(io_error(
            "closing an event stream requires the real executor's runtime".to_string(),
        ))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let sse_conns = self.sse_conns.clone();
        let conn = self.conn;
        Some(RealBody::Async(Box::pin(async move {
            // Idempotent: closing an already-closed stream is not an error (the serve loop closes
            // a session that may have closed itself).
            let Some(mut stream) = sse_conns.lock().unwrap().remove(&conn) else {
                return Ok(NativeOut::Unit);
            };
            let _ = stream.write_all(b"0\r\n\r\n").await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
            Ok(NativeOut::Unit)
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_is_hex_framed() {
        assert_eq!(chunk("data: hi\n\n"), b"a\r\ndata: hi\n\n\r\n".to_vec());
        // Ten bytes is `a`, sixteen is `10` — hex, not decimal. Getting this wrong desynchronizes
        // the whole stream, and a client reports it as a protocol error far from the cause.
        assert_eq!(&chunk(&"x".repeat(16))[..4], b"10\r\n");
        assert_eq!(chunk(""), b"0\r\n\r\n".to_vec());
    }

    #[test]
    fn the_head_declares_an_unbuffered_chunked_event_stream() {
        let head = stream_head();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("content-type: text/event-stream\r\n"));
        assert!(head.contains("cache-control: no-cache\r\n"));
        assert!(head.contains("transfer-encoding: chunked\r\n"));
        assert!(head.contains("x-accel-buffering: no\r\n"));
        assert!(
            !head.to_ascii_lowercase().contains("content-length"),
            "a stream has no length"
        );
        assert!(head.ends_with("\r\n\r\n"));
    }
}
