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

mod common;

/// The port every client fixture here is pinned to on disk, and the needle each test replaces with
/// the port its stub server actually got. Keeping a real one in the file is what makes the fixture a
/// program `tests/fixtures.rs` can compile rather than a template.
const FIXTURE_PORT: &str = "127.0.0.1:8080";

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
    // `tests/fixtures/live_stream/events.noe`. All four programs in this suite are on disk: this
    // test is `#[ignore]`d, so a literal here would never be compiled by `cargo test`, while a file
    // is compiled by `tests/fixtures.rs` on every run.
    std::fs::write(&program, common::fixture("live_stream/events"))
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
        common::fixture_with(
            "live_stream/read",
            &[(FIXTURE_PORT, &format!("127.0.0.1:{port}"))],
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
        common::fixture_with(
            "live_stream/status",
            &[(FIXTURE_PORT, &format!("127.0.0.1:{port}"))],
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

/// A session learns its client is gone and stops, instead of pushing into a socket nobody holds.
///
/// The handler below is scripted for far more ticks than the test waits for, and the client leaves
/// after the first couple of frames. What the session does next is the whole assertion: with
/// `sink.closed()` answering, it ends within a tick or two of the departure and its connection is
/// released; without it, `sink.send` keeps succeeding silently and the handler runs its entire
/// budget with nobody on the other end, holding the connection for the life of the process.
///
/// The tick it stops at is what is asserted, rather than elapsed time: detection costs one write,
/// so the number does not grow when the machine is busy.
#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn a_session_stops_when_its_client_goes_away() {
    /// Ticks the handler is willing to send, far past where the client leaves.
    const BUDGET: u32 = 40;
    /// How late the session may notice. One write is what detection costs; the slack is for a
    /// frame already in flight when the client left.
    const SLACK: u32 = 10;

    let dir = scratch("abandoned");
    let program = dir.join("abandoned.noe");
    std::fs::write(
        &program,
        common::fixture_with(
            "live_stream/abandoned",
            &[("i < 40", &format!("i < {BUDGET}"))],
        ),
    )
    .expect("write the fixture program");

    let port = noeta_test_temp::free_port();
    let log = noeta_test_temp::ServerLog::new("live-stream-abandoned");
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
        // Read far enough to know the session is running, then leave the way a killed client does.
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4096];
        let read = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the server closed the event stream before sending anything".to_string());
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
        drop(stream);

        // The session's own words, read from the server log as it writes them. The budget bounds
        // this: a handler that never notices says so at roughly `BUDGET` ticks, and the wait is
        // long enough to let it.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let said = std::fs::read_to_string(log.path()).unwrap_or_default();
            if let Some(line) = said.lines().find(|l| l.contains("session ended at tick")) {
                let tick: u32 = line
                    .rsplit(' ')
                    .next()
                    .and_then(|n| n.trim().parse().ok())
                    .ok_or_else(|| format!("no tick number in {line:?}"))?;
                if tick > SLACK {
                    return Err(format!(
                        "the session ran {tick} ticks past a client that left after the first \
                         frames (at most {SLACK} expected)"
                    ));
                }
                return Ok(());
            }
            if said.contains("session ran its whole budget") {
                return Err(format!(
                    "the session pushed all {BUDGET} frames into a closed connection: \
                     `sink.closed()` never reported the departed client"
                ));
            }
            if std::time::Instant::now() > deadline {
                return Err("the session neither ended nor finished its budget".to_string());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| panic!("{}", log.explain(format!("the abandoned stream: {e}"))));
}
