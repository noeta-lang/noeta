//! LiveView end-to-end over a real socket (server-hmr L2): serve the bundled counter example,
//! fetch the page and the bundled client shim over plain HTTP, then speak the view/diff protocol
//! through a real RFC 6455 websocket — snapshot on connect, JSON events in, minimal patch frames
//! out. `#[ignore]` so CI stays hermetic (real port, real process) — run explicitly:
//! `cargo test -p noeta-cli --test live_serve -- --ignored`.

mod common;

use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{get, ws_connect, ws_recv, ws_send};

#[test]
#[ignore = "spawns the CLI and binds a real socket; run explicitly"]
fn the_liveview_example_pushes_snapshot_and_patches_to_a_real_client() {
    let example = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/liveview_counter.noe");
    let port = 8473;
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args([
            "serve",
            example.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        let mut up = false;
        for _ in 0..80 {
            if TcpStream::connect(&addr).is_ok() {
                up = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !up {
            return Err("server did not accept within 4s".to_string());
        }

        // Plain HTTP: the page carries the bindings and loads the shim; the shim is served
        // source-intact from /live.js.
        let page = get(&addr, "/")?;
        for marker in [
            "data-live=\"count\"",
            "data-live-click=\"increment\"",
            "/live.js",
        ] {
            if !page.contains(marker) {
                return Err(format!("page lost `{marker}`:\n{page}"));
            }
        }
        let shim = get(&addr, "/live.js")?;
        if !shim.contains("noetaLive") {
            return Err(format!("shim not served: {shim}"));
        }

        // The protocol: snapshot on connect, then one minimal patch per event.
        let mut ws = ws_connect(&addr, "/ws")?;
        let (op, snapshot) = ws_recv(&mut ws)?;
        if op != 1 || snapshot != r#"{"type":"snapshot","values":{"count":0,"double":0}}"# {
            return Err(format!("bad snapshot: op={op} {snapshot}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"increment"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":1,"double":2}}"# {
            return Err(format!("bad first patch: {patch}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"increment"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":2,"double":4}}"# {
            return Err(format!("bad second patch: {patch}"));
        }
        ws_send(&mut ws, r#"{"type":"event","name":"reset"}"#)?;
        let (_, patch) = ws_recv(&mut ws)?;
        if patch != r#"{"type":"patch","changes":{"count":0,"double":0}}"# {
            return Err(format!("bad reset patch: {patch}"));
        }

        // A second session shares the signals: its snapshot sees the (reset) current state.
        let mut ws2 = ws_connect(&addr, "/ws")?;
        let (_, snap2) = ws_recv(&mut ws2)?;
        if snap2 != r#"{"type":"snapshot","values":{"count":0,"double":0}}"# {
            return Err(format!("bad second-session snapshot: {snap2}"));
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    outcome.expect("liveview end-to-end");
}
