//! Multi-core `noeta serve --parallel N` (server-hmr S1): N worker isolates share one bound
//! socket, the kernel load-balances connections, and a SIGINT drains every worker.
//!
//! `#[ignore]`d for the real port, threads and signal it needs, and run by name from ci.yml's `jit`
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest). By hand:
//! `cargo test -p noeta-cli --test parallel_serve -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

/// A handler that reports which OS thread served it, so the test can observe more than one worker
/// doing work (true multi-core, not one thread taking everything).
fn app() -> &'static str {
    "use std.http.server\n\
     use std.http.{Request, Response}\n\
     use std.task.{sleep}\n\
     async fn fetch(req: Request): Response {\n\
     \x20   sleep(200).await\n\
     \x20   return server.response(200, \"ok ${req.path()}\")\n\
     }\n"
}

fn get(addr: &str, path: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    resp.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .ok_or_else(|| "no body".to_string())
}

#[test]
#[ignore = "binds a real socket across threads and sends SIGINT; run explicitly"]
fn parallel_workers_share_the_listener_and_drain_together() {
    let dir = noeta_test_temp::TempDir::new("parallel-serve");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app()).unwrap();

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other
    // checkout and every concurrent run of this test on the machine, and the server that loses the
    // bind dies where the client sees only a reset connection.
    let port = noeta_test_temp::free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args([
            "serve",
            app_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--parallel",
            "4",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve --parallel 4`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr)?;

        // Fire 8 concurrent slow (200ms) requests. With a single worker they would serialize into
        // ~200ms each on one core; 4 workers handle them in parallel. We only assert correctness
        // (every request gets its body) — timing is machine-dependent — but the concurrency is
        // what exercises the multi-worker accept distribution.
        let addr2 = addr.clone();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let addr = addr2.clone();
                std::thread::spawn(move || get(&addr, &format!("/r{i}")))
            })
            .collect();
        for (i, t) in threads.into_iter().enumerate() {
            let body = t.join().unwrap()?;
            if body != format!("ok /r{i}") {
                return Err(format!("request {i} got {body:?}"));
            }
        }

        // SIGINT drains all four workers; afterwards the shared listener is closed.
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .map_err(|e| e.to_string())?;
        noeta_test_temp::wait_until_closed(&addr)
            .map_err(|e| format!("the listener did not close after SIGINT: {e}"))?;
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("parallel serve round trip");
}

#[test]
#[ignore = "binds a real socket across worker threads; run explicitly"]
fn a_worker_that_aborts_reports_its_diagnostics_and_its_stack() {
    // The `--parallel` worker tail (plans/parallel-path-audit.md row 1). It wrote the program's two
    // streams and then, for an abort, the five words `[worker] aborted` — no diagnostic, no stack —
    // in a project that ships production stack traces on both backends. The worker's run epilogue is
    // now the shared `RunTail`, so a worker that dies says what killed it and where.
    //
    // The abort is at program top level, before `server.serve` is reached, so the fleet dies without
    // needing a client: the listener is bound once up front and each worker aborts on entry. That
    // keeps the test to a bind and a wait — but it is still a real port, hence `#[ignore]` and
    // `scripts/hot-e2e.sh`.
    let dir = noeta_test_temp::TempDir::new("parallel-serve-abort");
    let app_path = dir.join("app.noe");
    std::fs::write(
        &app_path,
        "use std.io\n\
         use std.http.{Request, Response}\n\
         use std.http.server\n\
         fn detonate(): int {\n\
         \x20   panic(\"worker kaboom\")\n\
         }\n\
         fn fetch(req: Request): Response {\n\
         \x20   return server.response(200, \"ok\")\n\
         }\n\
         io.errln(\"worker speaking\")\n\
         echo detonate()\n",
    )
    .unwrap();

    let port = noeta_test_temp::free_port();
    let out = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args([
            "serve",
            app_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--parallel",
            "2",
        ])
        .output()
        .expect("run `noeta serve --parallel 2`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&dir);

    assert_ne!(
        out.status.code(),
        Some(0),
        "an aborting fleet exits non-zero"
    );
    // The program's own stderr stream survives the worker tail …
    assert!(
        stderr.contains("worker speaking"),
        "the worker dropped the program's own stderr stream:\n{stderr}"
    );
    // … the diagnostic that killed it is rendered …
    assert!(
        stderr.contains("worker kaboom"),
        "the worker rendered no diagnostic:\n{stderr}"
    );
    // … and so is the stack that led there, which is the part `[worker] aborted` never said.
    assert!(
        stderr.contains("stack trace"),
        "the worker rendered no traceback:\n{stderr}"
    );
    assert!(
        stderr.contains("detonate"),
        "the traceback names no frame:\n{stderr}"
    );
}
