//! `noeta serve` integration test (http-server S4): spawn the CLI serving a real handler, drive a
//! real HTTP request over a loopback socket, and assert the routed response.
//!
//! `#[ignore]`d because it binds a real port and spawns a process — a plain `cargo test` should not
//! do that behind your back. It still runs on every CI run and every merge gate: ci.yml's `jit` job
//! and `scripts/gate.sh`'s `serve` group name it, and `tests/cli/automation.rs` fails the build if
//! they stop. By hand: `cargo test -p noeta-cli --test serve -- --ignored`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn serve_routes_a_real_request() {
    // A tiny handler app: `fetch` routes on the path, building responses with `server.response`.
    // `noeta serve` synthesizes `server.serve(<port>, fetch)`, so the program supplies `fetch` and
    // `use std.http.server` (binding the local `server`); the `Request`/`Response` types are imported
    // like any native type.
    // The program file goes in a per-process fixture directory. It used to be one fixed name under
    // the system temp dir, which every checkout and every concurrent run of this test shared — and
    // the teardown below `remove_file`s it, so one run could delete the program another run's server
    // had not finished reading.
    let dir = noeta_test_temp::TempDir::new("serve-app");
    let app = dir.join("app.noe");
    std::fs::write(
        &app,
        "use std.http.server\n\
         use std.http.{Request, Response}\n\n\
         fn fetch(req: Request): Response {\n\
         \x20   if req.path() == \"/hi\" { return server.response(200, \"pong\") }\n\
         \x20   return server.response(404, \"nope\")\n\
         }\n",
    )
    .unwrap();

    // The *other* machine-global resource, and the one a per-process fixture directory cannot fix: a
    // fixed port. Two concurrent runs of this test had the second server fail to bind 8231 and die,
    // surfacing on the client side as `Connection reset by peer` and naming nothing — see `free_port`.
    let port = noeta_test_temp::free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["serve", app.to_str().unwrap(), "--port", &port.to_string()])
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve`");

    // Wait (up to ~2.5s) for the listener to accept.
    let addr = format!("127.0.0.1:{port}");
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = std::net::TcpStream::connect(&addr) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let outcome = (|| {
        let stream = stream.ok_or("server did not accept within 2.5s")?;
        // A hostile/empty probe first: connect and close without sending a request (what a port
        // scan or a load balancer's TCP health check does). The accept leaf must absorb it and
        // keep serving — propagating the wire error killed the whole server once (E0021).
        drop(stream);
        std::thread::sleep(Duration::from_millis(100));
        let mut stream = std::net::TcpStream::connect(&addr)
            .map_err(|e| format!("server died after an empty probe: {e}"))?;
        stream
            .write_all(b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut resp = String::new();
        stream
            .read_to_string(&mut resp)
            .map_err(|e| e.to_string())?;
        Ok::<String, String>(resp)
    })();

    // Always tear the server down, whatever the assertion outcome. The program file goes with `dir`.
    let _ = child.kill();
    let _ = child.wait();

    let resp = outcome.expect("request/response round trip");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    assert!(resp.trim_end().ends_with("pong"), "got: {resp}");
}
