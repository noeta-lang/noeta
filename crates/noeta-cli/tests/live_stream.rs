//! Streaming HTTP end-to-end over real sockets — the only place the *real*
//! host's streaming is exercised, since the conformance corpus always runs the deterministic
//! sandbox.
//!
//! Two directions, one per test:
//!
//!   - **serving** an event stream: a `server.sse` handler behind `noeta serve`, read by a raw TCP
//!     client that asserts the chunked `text/event-stream` framing byte for byte;
//!   - **reading** one incrementally: a hand-rolled TCP server that dribbles an SSE body out in
//!     several writes with pauses between them, consumed by `client.stream` in a real `noeta run`.
//!
//! The reading test is the one that would catch a body that only works when it arrives all at
//! once — it deliberately splits a frame across two writes, and splits one CRLF down the middle.
//!
//! `#[ignore]`d for the real ports and processes these need, and listed in `scripts/hot-e2e.sh`,
//! which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest).
//! By hand: `cargo test -p noeta-cli --test live_stream -- --ignored`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

/// A scratch directory for one fixture program, private to this process and this call.
///
/// Not the shared system temp dir: a Noeta entry point pulls in its *siblings* (the loader links
/// the containing directory as the project), so a fixture sharing a directory with another
/// process's stray `.noe` files compiles them too. The guard removes the tree when it drops.
fn scratch(name: &str) -> noeta_test_temp::TempDir {
    noeta_test_temp::TempDir::new(&format!("live-stream-{name}"))
}

/// Read the whole response (head + body) until the peer closes.
fn read_to_end(stream: &mut TcpStream) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Undo HTTP/1.1 chunked framing, so the assertions can be about the event stream rather than the
/// transport. Stops at the terminating zero-length chunk.
fn dechunk(body: &str) -> Result<String, String> {
    let mut rest = body;
    let mut out = String::new();
    loop {
        let (size, tail) = rest
            .split_once("\r\n")
            .ok_or_else(|| format!("truncated chunk header in {body:?}"))?;
        let size = usize::from_str_radix(size.trim(), 16)
            .map_err(|e| format!("bad chunk size {size:?}: {e}"))?;
        if size == 0 {
            return Ok(out);
        }
        if tail.len() < size {
            return Err(format!("chunk claims {size} bytes, {} left", tail.len()));
        }
        out.push_str(&tail[..size]);
        rest = tail[size..]
            .strip_prefix("\r\n")
            .ok_or_else(|| "chunk not terminated by CRLF".to_string())?;
    }
}

/// A `server.sse` handler, served for real, produces a correctly framed and correctly encoded
/// event stream: a chunked `text/event-stream` head, then the frames and the keepalive comment.
#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn a_served_sse_handler_streams_real_events() {
    let dir = scratch("serve");
    let program = dir.join("events.noe");
    std::fs::write(
        &program,
        r#"use std.http.server
use std.http.{Request, Response, SseSink, Frame}

async fn events(sink: SseSink): bool {
    sink.send(Frame { event: "start", data: "go", id: "1", retry: none })
    sink.comment("keepalive")
    sink.send(Frame { event: "", data: "one\ntwo", id: "", retry: none })
    sink.send(Frame { event: "end", data: "[DONE]", id: "2", retry: some(1500) })
    return true
}

fn fetch(req: Request): Response {
    if req.path() == "/events" {
        return server.sse(events)
    }
    return server.response(200, "ok")
}
"#,
    )
    .expect("write the fixture program");

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other
    // checkout and every concurrent run of this test on the machine, and the server that loses the
    // bind dies where the client sees only a reset connection.
    let port = noeta_test_temp::free_port();
    // The server's output goes to a file this test can quote rather than to `/dev/null` — see
    // `noeta_test_temp::ServerLog`, and the three investigations that line cost.
    let log = noeta_test_temp::ServerLog::new("live-stream");
    let mut child = log
        .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args([
            "serve",
            program.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ]))
        .expect("spawn `noeta serve`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        let mut stream = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        stream
            .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let response = read_to_end(&mut stream)?;

        let (head, body) = response
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("no head/body split in {response:?}"))?;
        let lower = head.to_ascii_lowercase();
        for marker in [
            "http/1.1 200 ok",
            "content-type: text/event-stream",
            "cache-control: no-cache",
            "transfer-encoding: chunked",
        ] {
            if !lower.contains(marker) {
                return Err(format!("head is missing `{marker}`:\n{head}"));
            }
        }
        if lower.contains("content-length") {
            return Err(format!("a stream must not declare a length:\n{head}"));
        }

        // The exact wire bytes: a multi-line payload becomes two `data:` lines (the only legal way
        // to carry a newline), and the comment carries no data so it dispatches no event.
        let events = dechunk(body)?;
        let expected = "event: start\nid: 1\ndata: go\n\n\
                        : keepalive\n\
                        data: one\ndata: two\n\n\
                        event: end\nid: 2\nretry: 1500\ndata: [DONE]\n\n";
        if events != expected {
            return Err(format!(
                "event stream mismatch:\n got: {events:?}\nwant: {expected:?}"
            ));
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| panic!("{}", log.explain(format!("the served event stream: {e}"))));
}

/// `client.stream` reads a real body **incrementally**: the server below writes the SSE body in
/// five pieces with pauses, splitting a frame across two writes and splitting one CRLF between
/// them, so a reader that only works on a whole buffered body fails here.
#[test]
#[ignore = "binds a real socket and runs the CLI; run explicitly"]
fn client_stream_reads_a_real_body_arriving_in_pieces() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub server");
    let port = listener.local_addr().expect("addr").port();

    // The stub: an SSE body dribbled out in pieces. The split points are the interesting part.
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        // Drain the request head so the client is not blocked writing.
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf);
        let pieces: [&[u8]; 6] = [
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            b"event: token\r\ndata: He",
            // The rest of the frame, and the FIRST HALF of the terminating CRLF pair.
            b"llo\r",
            b"\n\r\n",
            // A frame split mid-field, then the terminator.
            b"data: wor",
            b"ld\n\ndata: [DONE]\n\n",
        ];
        for piece in pieces {
            if socket.write_all(piece).is_err() {
                return;
            }
            let _ = socket.flush();
            std::thread::sleep(Duration::from_millis(30));
        }
    });

    let dir = scratch("read");
    let program = dir.join("read.noe");
    std::fs::write(
        &program,
        format!(
            r#"use std.http.client
use std.http.Framing
use std.http.Frame
use std.http.HttpError

fn blank(): Frame {{
    return Frame {{ event: "", data: "", id: "", retry: none }}
}}

// `Result<void, HttpError>`, not `void`: the `?` below early-returns a failed open, so the signature
// has to be able to carry one (E0012). It also makes this fixture fail LOUDLY — a failed connection to
// the stub server aborts with the transport message and a non-zero exit instead of producing an empty
// stdout that only the `assert_eq!` below would notice.
async fn run(): Result<void, HttpError> {{
    api = client.new("http://127.0.0.1:{port}")
    stream = client.stream(api.prepare("get", "/events"), Framing.Sse)?
    // The head, straight off the real handshake and before any frame is consumed.
    echo "head ${{stream.status()}} ${{stream.ok()}} ${{stream.header("content-type") ?? "none"}}"
    mut going = true
    while going {{
        next = stream.recv().await
        if next == none {{
            going = false
        }} else {{
            f: Frame = next ?? blank()
            echo "[${{f.event}}] ${{f.data}}"
        }}
    }}
    echo "done"
    return Ok()
}}
run().await?
"#
        ),
    )
    .expect("write the fixture program");

    let output = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["run", program.to_str().unwrap()])
        .output()
        .expect("run the streaming client");
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `Hello` proves the frame split across two writes reassembled, and that the CRLF split
    // between writes counted as ONE line ending — treating the lone `\r` as a terminator would
    // have dispatched the frame early as `He`.
    let expected = "head 200 true text/event-stream\n[token] Hello\n[] world\n[] [DONE]\ndone\n";
    assert_eq!(
        stdout, expected,
        "incremental read mismatch (stderr: {stderr})"
    );
}

/// A **non-2xx** streamed response over a real socket — the failure `FrameStream.status()` exists
/// for, on the path where it actually bites.
///
/// The server answers `429` with a bare JSON error document and a `retry-after`, which is what a
/// rate-limited provider really sends. It is not an event stream, so the SSE decoder cuts it into
/// **zero** frames — correctly. Before the head was carried, that made a rate limit and a model
/// with nothing to say the same observation, and the sandbox alone cannot prove the real host reads
/// the status off a live reqwest response.
#[test]
#[ignore = "binds a real socket and runs the CLI; run explicitly"]
fn client_stream_reads_the_head_of_a_real_rate_limited_response() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub server");
    let port = listener.local_addr().expect("addr").port();

    let body = r#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error"}}"#;
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf);
        let head = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\n\
             retry-after: 30\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes());
        let _ = socket.write_all(body.as_bytes());
        let _ = socket.flush();
    });

    let dir = scratch("status");
    let program = dir.join("status.noe");
    std::fs::write(
        &program,
        format!(
            r#"use std.http.client
use std.http.Framing
use std.http.HttpError

// `Result<void, HttpError>`, not `void`: the `?` below early-returns a failed open, so the signature
// has to be able to carry one (E0012) — and a fixture that could not connect at all now aborts with
// the transport message instead of quietly printing nothing.
async fn run(): Result<void, HttpError> {{
    api = client.new("http://127.0.0.1:{port}")
    // Opening SUCCEEDS: a status is an answer, not a transport failure, so `?` does not fire.
    stream = client.stream(api.prepare("post", "/v1/chat", "hi"), Framing.Sse)?
    echo "status ${{stream.status()}} ok=${{stream.ok()}} retry=${{stream.header("retry-after") ?? "none"}}"
    mut frames = 0
    mut going = true
    while going {{
        next = stream.recv().await
        if next == none {{
            going = false
        }} else {{
            frames = frames + 1
        }}
    }}
    echo "frames ${{frames}}"
    echo match stream.error_for_status() {{
        Ok(_) => "error_for_status: unexpectedly ok",
        Err(e) => "error_for_status ${{e.kind()}}",
    }}
    return Ok()
}}
run().await?
"#
        ),
    )
    .expect("write the fixture program");

    let output = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["run", program.to_str().unwrap()])
        .output()
        .expect("run the streaming client");
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stdout, "status 429 ok=false retry=30\nframes 0\nerror_for_status status\n",
        "the real host must report the streamed response head (stderr: {stderr})"
    );
}
