//! The L3 showcase, end to end: a LiveView client is connected while the developer edits the
//! server under `noeta serve --watch`. A hot-swappable edit pushes `{"type":"reload"}` to the
//! live socket and closes it; the reconnect lands in a fresh session running the NEW code whose
//! snapshot carries the PRESERVED signal state. A rejected (red-check) edit pushes an `error`
//! frame — the overlay — and keeps the socket open.
//!
//! `#[ignore]`d for the real port, processes and fs events it needs, and run by name from ci.yml's
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest).
//! By hand: `cargo test -p noeta-cli --test hot_live -- --ignored`.

mod common;

use std::process::{Command, Stdio};

use common::{ws_connect, ws_recv, ws_send};

/// The app: reactive state exposed through a view; any client frame increments.
fn app(double_factor: &str) -> String {
    format!(
        "use std.http.server\n\
         use std.http.{{Request, Response, Socket}}\n\
         use std.reactive.{{signal, computed, view}}\n\n\
         count = signal(0)\n\
         double = computed(fn() {{\n\
         \x20   return count.get() * {double_factor}\n\
         }})\n\n\
         async fn session(sock: Socket) use (count, double): bool {{\n\
         \x20   v = view()\n\
         \x20   v.expose(\"count\", count)\n\
         \x20   v.expose(\"double\", double)\n\
         \x20   sock.send(v.snapshot())\n\
         \x20   mut going = true\n\
         \x20   while going {{\n\
         \x20       msg = sock.recv().await\n\
         \x20       if msg == none {{\n\
         \x20           going = false\n\
         \x20       }} else {{\n\
         \x20           count.set(count.get() + 1)\n\
         \x20           patch = v.diff() ?? \"\"\n\
         \x20           if patch != \"\" {{\n\
         \x20               sock.send(patch)\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}\n\
         \x20   return true\n\
         }}\n\n\
         fn fetch(req: Request): Response {{\n\
         \x20   if req.path() == \"/ws\" {{\n\
         \x20       return server.websocket(session)\n\
         \x20   }}\n\
         \x20   return server.response(200, \"ok\")\n\
         }}\n"
    )
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args([
            "serve",
            "--watch",
            app_path.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .current_dir(&dir)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr)?;

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
    let _ = std::fs::write(dir.join("teardown.noe"), "// trigger child exit\n");
    noeta_test_temp::settle_closed(&addr);
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("liveview hot-reload round trip");
}
