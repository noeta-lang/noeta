//! `noeta serve` integration test: spawn the CLI serving a real handler, drive a
//! real HTTP request over a loopback socket, and assert the routed response.
//!
//! `#[ignore]`d because it binds a real port and spawns a process — a plain `cargo test` should not
//! do that behind your back. It still runs on every CI run and every merge gate: it is listed in
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run, and `tests/cli/automation.rs`
//! fails the build if that list drops it. By hand:
//! `cargo test -p noeta-cli --test serve -- --ignored`.

use std::io::{Read, Write};
use std::process::Command;
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
    // The server's own two streams go to a file this test can quote, not to `/dev/null`. That one
    // line — `.stderr(Stdio::null())` — is what turned three separate startup failures into
    // investigations of the *symptom*; see `noeta_test_temp::ServerLog`.
    let log = noeta_test_temp::ServerLog::new("serve");
    let mut child = log
        .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args([
            "serve",
            app.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ]))
        .expect("spawn `noeta serve`");

    // Wait for the listener to accept — see `noeta_test_temp::wait_until_listening`, which is where
    // the budget for that lives now (it used to be 2.5s here and 4s in every sibling suite).
    let addr = format!("127.0.0.1:{port}");
    let stream = noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log);
    let outcome = (|| {
        let stream = stream?;
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

    // Every way this test can fail carries the server's output, not only the readiness wait: the
    // worst of the three incidents got *past* readiness (a port race whose loser probed the
    // winner's identical server) and failed on the request below.
    let resp = outcome.unwrap_or_else(|e| {
        panic!(
            "{}",
            log.explain(format!("request/response round trip: {e}"))
        )
    });
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    assert!(resp.trim_end().ends_with("pong"), "got: {resp}");
}

/// **The regression test for the null'd stderr, shape one: the server dies on startup.**
///
/// A fixture program that does not check. The server prints `[E0005] cannot find …` and exits, and
/// the readiness wait reports it — that is the shape `hot_serve` and `hot_live` failed in for
/// *weeks* on `main`, reporting `server did not accept within 4s` while the sentence naming the
/// cause went to `/dev/null` (`plans/backlog.md`; `noeta_test_temp::ServerLog`, incident 1).
///
/// The assertion is on the message the suite prints, and it asks for both halves: the fact (the
/// process exited) and the cause (what it said on the way out). Before the capture, the message
/// stopped after the fact.
#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn a_server_that_dies_on_startup_is_quoted_in_the_readiness_failure() {
    let dir = noeta_test_temp::TempDir::new("serve-red");
    let app = dir.join("app.noe");
    // `nope()` does not exist: `noeta serve` checks before it binds, so this never reaches a socket.
    std::fs::write(
        &app,
        "use std.http.server\n\
         use std.http.{Request, Response}\n\n\
         fn fetch(req: Request): Response {\n\
         \x20   return server.response(200, nope())\n\
         }\n",
    )
    .unwrap();

    let port = noeta_test_temp::free_port();
    let log = noeta_test_temp::ServerLog::new("serve-red");
    let mut child = log
        .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args([
            "serve",
            app.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ]))
        .expect("spawn `noeta serve`");

    let addr = format!("127.0.0.1:{port}");
    let err = noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)
        .err()
        .unwrap_or_else(|| panic!("a program that does not check cannot have served {addr}"));
    let _ = child.kill();
    let _ = child.wait();

    // The fact — this half already worked.
    assert!(
        err.contains("exited") && err.contains("never came up"),
        "the message no longer names the exit: {err}"
    );
    // The cause — this half is the defect. `[E0005] cannot find `nope` in this scope`, in the
    // failure that reports it, instead of on a discarded stream.
    assert!(
        err.contains("E0005") && err.contains("cannot find `nope`"),
        "the server's own diagnostic is missing from the failure it caused: {err}"
    );
    // Attributed, so a reader knows these words are the server's rather than the harness's guess.
    assert!(
        err.contains("what the server itself said"),
        "the quoted output is unattributed: {err}"
    );
}

/// **The regression test for the null'd stderr, shape two: the server loses the bind.**
///
/// This is incident 3, reproduced deliberately, and it is the harder shape — *readiness succeeds*.
/// The port is held first, so the spawned server dies with `[E0021] cannot bind …: Address already
/// in use`; but an occupied port is a port that **accepts**, so the readiness probe completes
/// against the squatter exactly as the losing test's probe completed against the winner's identical
/// fixture server. Nothing is wrong until a request is made, which is where the original incident
/// surfaced too — as a bare `Connection refused`, naming nothing, blamed for a whole afternoon on a
/// readiness budget that had not expired.
///
/// So this test pins the *other* half of the fix: the log is quoted at the suite's boundary, where
/// `outcome.unwrap_or_else(|e| panic!("{}", log.explain(e)))` catches every failure path and not
/// only the readiness wait. The string asserted on is the exact expression each suite now panics
/// with.
#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn a_server_that_loses_the_bind_is_quoted_even_though_readiness_succeeds() {
    let dir = noeta_test_temp::TempDir::new("serve-clash");
    let app = dir.join("app.noe");
    std::fs::write(
        &app,
        "use std.http.server\n\
         use std.http.{Request, Response}\n\n\
         fn fetch(req: Request): Response {\n\
         \x20   return server.response(200, \"never reached\")\n\
         }\n",
    )
    .unwrap();

    // The squatter: this test in the role of the *winner* of the port race. It accepts and drops,
    // which is enough to satisfy any readiness probe.
    //
    // The port comes from the bind itself (`:0`) rather than from `free_port`, and that is not a
    // shortcut — it removes a window this test would otherwise lose to on its own machine.
    // `free_port` hands back a port it has *closed*, claimed only against other `free_port`
    // callers; the ephemeral range it draws from is also where every outbound connection in this
    // binary gets its source port, and a sibling test's client took one of those between the draw
    // and the bind (`Address already in use`, on the line meant to *create* that condition).
    // Binding straight to `:0` never lets go of the port at all.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = held
        .local_addr()
        .expect("the held socket has an address")
        .port();
    held.set_nonblocking(true).expect("non-blocking accept");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let accepting = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                // Accept and close: a connect succeeds, a request gets nothing back.
                drop(held.accept());
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    let log = noeta_test_temp::ServerLog::new("serve-clash");
    let mut child = log
        .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args([
            "serve",
            app.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ]))
        .expect("spawn `noeta serve`");

    let addr = format!("127.0.0.1:{port}");
    let outcome = (|| -> Result<(), String> {
        // Succeeds — against a stranger. That is the incident, not a flaw in the wait.
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        let mut stream = std::net::TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        stream
            .write_all(b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut resp = String::new();
        stream
            .read_to_string(&mut resp)
            .map_err(|e| e.to_string())?;
        if resp.is_empty() {
            // The other way a dropped connection shows up, depending on where the close lands: an
            // orderly EOF rather than a reset. Both are symptoms, and both used to be all a reader
            // got.
            return Err("the server closed the connection without replying".to_string());
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    accepting.join().expect("the squatter thread ends");

    let err = outcome.expect_err("the squatter cannot answer a request");
    let message = log.explain(format!("request/response round trip: {err}"));
    // The symptom is still there — whichever of the two a dropped connection produced (`Connection
    // reset by peer` when the squatter's close raced the request, an empty read when it did not).
    assert!(
        message.starts_with("request/response round trip: "),
        "the symptom was lost: {message}"
    );
    // …and now it arrives with the cause attached, which is the whole point.
    assert!(
        message.contains("E0021") && message.contains("Address already in use"),
        "the server said why it died and the failure did not repeat it: {message}"
    );
}
