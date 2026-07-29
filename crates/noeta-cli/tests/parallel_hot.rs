//! Multi-worker in-process hot reload (server-hmr F5): under `noeta serve --parallel N --watch`,
//! a source edit **broadcasts** to every worker isolate — each drains the shared swap queue and
//! serves the new code, no restart. `#[ignore]` (real port, real threads, real fs events):
//! `cargo test -p noeta-cli --test parallel_hot -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn app(tag: &str) -> String {
    format!(
        "use std.http.server\n\
         use std.http.{{Request, Response}}\n\
         fn fetch(req: Request): Response {{\n\
         \x20   return server.response(200, \"{tag}\")\n\
         }}\n"
    )
}

fn get(addr: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    resp.rsplit("\r\n\r\n")
        .next()
        .map(|b| b.trim_end().to_string())
        .ok_or_else(|| "no body".to_string())
}

#[test]
#[ignore = "spawns the CLI across threads and edits real files; run explicitly"]
fn an_edit_broadcasts_to_every_worker() {
    let dir = std::env::temp_dir().join(format!("noeta-parallel-hot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app("v1")).unwrap();

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
            "--parallel",
            "3",
        ])
        .current_dir(&dir)
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve --parallel 3 --watch`");
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
        // Many requests hit different workers; all serve v1.
        for _ in 0..12 {
            let r = get(&addr)?;
            if r != "v1" {
                return Err(format!("pre-edit expected v1, got {r:?}"));
            }
        }

        // Edit the handler; the swap must reach EVERY worker (not just one), so after it settles
        // every request — whichever worker answers — serves v2.
        std::fs::write(&app_path, app("v2")).map_err(|e| e.to_string())?;
        let mut all_v2 = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            // 12 requests fan across the 3 workers; require them ALL v2 before declaring success.
            let mut seen_v2 = true;
            for _ in 0..12 {
                if get(&addr)? != "v2" {
                    seen_v2 = false;
                    break;
                }
            }
            if seen_v2 {
                all_v2 = true;
                break;
            }
        }
        if !all_v2 {
            return Err("the edit did not broadcast to every worker".to_string());
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("parallel hot broadcast round trip");
}
