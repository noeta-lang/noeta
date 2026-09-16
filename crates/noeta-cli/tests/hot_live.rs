//! The LiveView showcase, end to end: a LiveView client is connected while the developer edits the
//! server under `noeta serve --watch`. A hot-swappable edit pushes `{"type":"reload"}` to the
//! live socket and closes it; the reconnect lands in a fresh session running the NEW code whose
//! snapshot carries the PRESERVED signal state. A rejected (red-check) edit pushes an `error`
//! frame — the overlay — and keeps the socket open.
//!
//! `#[ignore]`d for the real port, processes and fs events it needs, and run by name from ci.yml's
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest).
//! By hand: `cargo test -p noeta-cli --test hot_live -- --ignored`.

mod common;

use std::process::Command;

use common::{ws_connect, ws_recv, ws_send};

/// The app: reactive state exposed through a view; any client frame increments.
///
/// The program is `tests/fixtures/hot_live/app.noe`. An inline literal would be compiled by no
/// `cargo test` at all, since `#[ignore]` switches off the only run that would reach it; on disk it
/// goes through `tests/fixtures.rs` every time. `double_factor` is substituted into the fixture's own
/// `* 2`, so the file on disk stays a program rather than a template.
///
/// One of this test's four versions is `"boom"`, a type error the server is *meant* to reject, so it
/// is the substitution that carries it and never the fixture.
fn app(double_factor: &str) -> String {
    common::fixture_with(
        "hot_live/app",
        &[("count.get() * 2", &format!("count.get() * {double_factor}"))],
    )
}

/// The file written outside the entry to make the server child's watcher exit at teardown.
fn teardown() -> String {
    common::fixture("hot_live/teardown")
}

#[test]
#[ignore = "spawns the CLI, binds a real socket, and writes real files; run explicitly"]
fn a_live_client_gets_reload_on_swap_and_error_on_red_check() {
    let dir = noeta_test_temp::TempDir::new("hot-live");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app("2")).unwrap();

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other
    // checkout and every concurrent run of this test on the machine, and the server that loses the
    // bind dies where the client sees only a reset connection.
    let port = noeta_test_temp::free_port();
    // The watch wrapper's output goes to a file this test can quote rather than to `/dev/null`.
    // This suite is one of the two that sat red on `main` for weeks over an `E0005` printed there
    // (`noeta_test_temp::ServerLog`) — and it is also the one that deliberately drives the server
    // into a RED CHECK, so its log is where that diagnostic goes.
    let log = noeta_test_temp::ServerLog::new("hot-live");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args([
                    "serve",
                    "--watch",
                    app_path.to_str().unwrap(),
                    "--port",
                    &port.to_string(),
                ])
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;

        // A live session builds up signal state.
        let mut ws = ws_connect(&addr, "/ws")?;
        let (_, snap) = ws_recv(&mut ws)?;
        if snap != r#"{"type":"snapshot","values":{"count":0,"double":0}}"# {
            return Err(format!("bad snapshot: {snap}"));
        }
        ws_send(&mut ws, "tick")?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":1,"double":2}}"# {
            return Err(format!("bad patch: {patch}"));
        }

        // THE SHOWCASE — edit the computed's formula. The idle server applies the swap on the
        // watcher's wake (no request needed) and pushes reload + close to the live socket.
        std::fs::write(&app_path, app("10")).map_err(|e| e.to_string())?;
        let (op, frame) = ws_recv(&mut ws)?;
        if op != 1 || frame != r#"{"type":"reload"}"# {
            return Err(format!("expected reload, got op={op} {frame}"));
        }
        let (op, _) = ws_recv(&mut ws)?;
        if op != 8 {
            return Err(format!("expected close after reload, got opcode {op}"));
        }

        // The reconnect (the shim's reload → new page → new socket): fresh session, NEW code,
        // PRESERVED count.
        let mut ws = ws_connect(&addr, "/ws")?;
        let (_, snap) = ws_recv(&mut ws)?;
        if snap != r#"{"type":"snapshot","values":{"count":1,"double":10}}"# {
            return Err(format!(
                "state did not survive the swap into new code: {snap}"
            ));
        }

        // A red-check edit (`count.get() * "boom"` — a type error; an unknown NAME would check
        // green, a separate checker gap): the error frame arrives on the OPEN socket, no close —
        // the old version keeps serving under the overlay.
        std::fs::write(&app_path, app("\"boom\"")).map_err(|e| e.to_string())?;
        let (op, frame) = ws_recv(&mut ws)?;
        if op != 1 || !frame.starts_with(r#"{"type":"error","message":""#) {
            return Err(format!("expected error frame, got op={op} {frame}"));
        }

        // Fixing it (to a NEW version — rewriting the old bytes would diff as Unchanged) swaps
        // and reloads the same socket, proving it stayed open through the error.
        std::fs::write(&app_path, app("20")).map_err(|e| e.to_string())?;
        let (_, frame) = ws_recv(&mut ws)?;
        if frame != r#"{"type":"reload"}"# {
            return Err(format!("expected reload after the fix, got {frame}"));
        }
        let mut ws = ws_connect(&addr, "/ws")?;
        let (_, snap) = ws_recv(&mut ws)?;
        if snap != r#"{"type":"snapshot","values":{"count":1,"double":20}}"# {
            return Err(format!("bad post-fix snapshot: {snap}"));
        }
        Ok(())
    })();

    // Teardown: kill the wrapper FIRST (so nothing respawns), then a change outside the entry
    // file makes the server child's hot watcher exit with the restart sentinel.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::write(dir.join("teardown.noe"), teardown());
    noeta_test_temp::settle_closed(&addr);
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| {
        panic!(
            "{}",
            log.explain(format!("liveview hot-reload round trip: {e}"))
        )
    });
}
