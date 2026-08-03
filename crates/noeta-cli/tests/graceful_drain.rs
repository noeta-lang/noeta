//! Graceful drain + `--host` (server-hmr S0), end to end.
//!
//! `#[ignore]`d for the real port and signal it needs, and listed in `scripts/hot-e2e.sh`, which
//! both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest). By
//! hand: `cargo test -p noeta-cli --test graceful_drain -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

fn app() -> &'static str {
    "use std.http.server\n\
     use std.http.{Request, Response}\n\
     use std.task.{sleep}\n\
     async fn fetch(req: Request): Response {\n\
     \x20   sleep(400).await\n\
     \x20   return server.response(200, \"drained ${req.path()}\")\n\
     }\n"
}

/// A slow request on its own thread: connect, send, block reading the reply into `out`.
fn slow_request(addr: String) -> std::thread::JoinHandle<Result<String, String>> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        s.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut resp = String::new();
        s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
        Ok(resp)
    })
}

#[test]
#[ignore = "binds a real socket and sends SIGINT; run explicitly"]
fn sigint_drains_in_flight_requests_and_host_binds_local_only() {
    let dir = noeta_test_temp::TempDir::new("drain");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app()).unwrap();

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other
    // checkout and every concurrent run of this test on the machine, and the server that loses the
    // bind dies where the client sees only a reset connection.
    let port = noeta_test_temp::free_port();
    // The server's output goes to a file this test can quote rather than to `/dev/null` — see
    // `noeta_test_temp::ServerLog`, and the three investigations that line cost.
    let log = noeta_test_temp::ServerLog::new("drain");
    // --host 127.0.0.1 binds local-only (S0): the loopback connect must work.
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
        // Start a slow (400ms) request, give it time to arrive at the handler…
        let inflight = slow_request(addr.clone());
        std::thread::sleep(Duration::from_millis(150));

        // …then SIGINT the server mid-request. A graceful drain must still answer it. (Portable
        // `kill -INT` rather than a `libc` dep just for the test.)
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .map_err(|e| e.to_string())?;

        // The in-flight request completes with its real body (not a dropped connection).
        let resp = inflight.join().unwrap()?;
        if !resp.contains("drained /slow") {
            return Err(format!("in-flight request was not drained: {resp:?}"));
        }
        // After the drain, the listener is closed — a new connection is refused.
        noeta_test_temp::wait_until_closed(&addr)
            .map_err(|e| format!("the listener did not close after the drain: {e}"))?;
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    // Every failure path quotes the server, not only the readiness wait: a drain that never closes
    // the listener is exactly the case where the server is still talking.
    outcome
        .unwrap_or_else(|e| panic!("{}", log.explain(format!("graceful drain round trip: {e}"))));
}
