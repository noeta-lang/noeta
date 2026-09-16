//! **A program shuts its own server down**, end to end over a real socket.
//!
//! The sandbox corpus (`tests/conformance/http_shutdown/`) pins the semantics on both backends: an
//! `os.exit(code)` in a handler drains and ends the run with `code`. Two things it cannot observe
//! live here. The sandbox records replies in a transcript the differential never compares, so the
//! **503** the exiting request is answered with has no other home; and the sandbox has no process,
//! so nothing there proves the real `noeta serve` closes its listener and exits with the code the
//! handler asked for rather than serving on.
//!
//! Serving on is what this actually guards. A handler's `os.exit` used to be recovered from as an
//! ordinary abort: the client got a 500, the code was latched where nothing read it, and the server
//! kept accepting. The program believed it had exited.
//!
//! `#[ignore]`d for the real port it binds, and listed in `scripts/hot-e2e.sh`, which both ci.yml
//! and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest). By hand:
//! `cargo test -p noeta-cli --test serve_exit -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

/// `/slow` takes 400 ms and replies normally. `/quit` ends the process with 17 and never replies on
/// its own, so the drain owes the client an answer and the in-flight `/slow` its real body.
fn app() -> &'static str {
    "use std.http.server\n\
     use std.http.{Request, Response}\n\
     use std.task.{sleep}\n\
     use std.{os}\n\
     async fn fetch(req: Request): Response {\n\
     \x20   if req.path() == \"/quit\" {\n\
     \x20       os.exit(17)\n\
     \x20   }\n\
     \x20   sleep(400).await\n\
     \x20   return server.response(200, \"drained ${req.path()}\")\n\
     }\n"
}

/// One request on its own thread: connect, send, block reading the whole reply.
fn request(addr: String, path: &'static str) -> std::thread::JoinHandle<Result<String, String>> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut resp = String::new();
        s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
        Ok(resp)
    })
}

#[test]
#[ignore = "binds a real socket; run explicitly"]
fn os_exit_in_a_handler_drains_the_server_and_ends_the_process_with_its_code() {
    let dir = noeta_test_temp::TempDir::new("serve-exit");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app()).unwrap();

    // A kernel-assigned port: a fixed one is shared with every other checkout and every concurrent
    // run on the machine, and the server that loses the bind dies where the client sees a reset.
    let port = noeta_test_temp::free_port();
    let log = noeta_test_temp::ServerLog::new("serve-exit");
    let mut child = log
        .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args([
            "serve",
            app_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--host",
            "127.0.0.1",
        ]))
        .expect("spawn `noeta serve`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;

        // A slow request first, given time to reach its `await`…
        let inflight = request(addr.clone(), "/slow");
        std::thread::sleep(Duration::from_millis(150));

        // …then the one whose handler ends the process. It produced no response of its own, and
        // the server is going away, so the drain answers it 503 rather than claiming a failure.
        let quit = request(addr.clone(), "/quit").join().unwrap()?;
        if !quit.contains("503") {
            return Err(format!("`/quit` was not answered 503: {quit:?}"));
        }

        // The request already in flight still completes, with its real body. This is the half that
        // makes the shutdown graceful rather than an abort.
        let slow = inflight.join().unwrap()?;
        if !slow.contains("drained /slow") {
            return Err(format!("the in-flight request was not drained: {slow:?}"));
        }

        // The listener is closed: a new connection is refused.
        noeta_test_temp::wait_until_closed(&addr)
            .map_err(|e| format!("the listener did not close after the drain: {e}"))?;

        // And the process is gone, carrying the code the handler asked for. A server that kept
        // serving, or that ended on the abort path instead, fails here.
        let status = child.wait().map_err(|e| e.to_string())?;
        if status.code() != Some(17) {
            return Err(format!(
                "`noeta serve` exited {:?}, not the requested 17",
                status.code()
            ));
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    // Quote the server on every failure path: a drain that never ends is exactly the case where the
    // server is still talking.
    outcome.unwrap_or_else(|e| panic!("{}", log.explain(format!("serve self-shutdown: {e}"))));
}
