//! `noeta serve --watch` hot-reload integration test (server-hmr W1): spawn the watch wrapper
//! around a stateful server, edit the handler mid-flight, and assert that (a) the edit's new body
//! serves without a restart and (b) the signal-held request counter SURVIVES the swap — the arc's
//! headline behavior.
//!
//! `#[ignore]`d because it binds a real port and spawns real processes — not because it is optional.
//! `scripts/hot-e2e.sh` lists it, and both ci.yml and `scripts/gate.sh` run that script; the census
//! in `tests/cli/automation.rs` fails the build if the list drops it. Run it yourself with
//! `cargo test -p noeta-cli --test hot_serve -- --ignored`.

use std::io::{Read, Write};
use std::process::Command;
use std::time::Duration;

fn get(addr: &str) -> Result<String, String> {
    let mut stream = std::net::TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| e.to_string())?;
    resp.rsplit("\r\n\r\n")
        .next()
        .map(|b| b.trim_end().to_string())
        .ok_or_else(|| "no body".to_string())
}

#[test]
#[ignore = "spawns the CLI, binds a real socket, and writes real files; run explicitly"]
fn a_hot_swap_preserves_signal_state_across_a_handler_edit() {
    let dir = noeta_test_temp::TempDir::new("hot-serve");
    let app = dir.join("app.noe");
    let v = |tag: &str| {
        format!(
            "use std.http.server\n\
             use std.http.{{Request, Response}}\n\
             use std.reactive.{{signal}}\n\n\
             count = signal(0)\n\n\
             fn fetch(req: Request) use (count): Response {{\n\
             \x20   count.set(count.get() + 1)\n\
             \x20   return server.response(200, \"{tag} hits=${{count.get()}}\")\n\
             }}\n"
        )
    };
    std::fs::write(&app, v("v1")).unwrap();

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other
    // checkout and every concurrent run of this test on the machine, and the server that loses the
    // bind dies where the client sees only a reset connection.
    let port = noeta_test_temp::free_port();
    // The wrapper's output — and that of every server generation it respawns, which inherit the
    // same file — goes somewhere this test can quote it, rather than to `/dev/null`. This suite is
    // the reason the capture exists: it sat red on `main` for weeks reporting a readiness timeout,
    // over an `E0005` the fixture program printed where nobody could read it
    // (`noeta_test_temp::ServerLog`).
    let log = noeta_test_temp::ServerLog::new("hot-serve");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args([
                    "serve",
                    "--watch",
                    app.to_str().unwrap(),
                    "--port",
                    &port.to_string(),
                ])
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        // Two requests against v1 build up signal state.
        let r1 = get(&addr)?;
        let r2 = get(&addr)?;
        if r1 != "v1 hits=1" || r2 != "v1 hits=2" {
            return Err(format!("unexpected v1 responses: {r1:?} {r2:?}"));
        }
        // Edit the handler body (a body-level change — hot-swappable).
        std::fs::write(&app, v("v2")).map_err(|e| e.to_string())?;
        // The swap lands at a scheduler tick during the next request(s); poll until the new
        // body serves. The counter must KEEP COUNTING across the swap — that is the state rule.
        let mut hits_seen = 2;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            let r = get(&addr)?;
            let (tag, hits) = r
                .split_once(" hits=")
                .ok_or_else(|| format!("malformed response: {r:?}"))?;
            let hits: i64 = hits.parse().map_err(|_| format!("malformed hits: {r:?}"))?;
            if hits != hits_seen + 1 {
                return Err(format!(
                    "counter did not survive: expected {} got {r:?}",
                    hits_seen + 1
                ));
            }
            hits_seen = hits;
            if tag == "v2" {
                return Ok(());
            }
        }
        Err("the edit never hot-swapped in".to_string())
    })();

    // Teardown: kill the wrapper FIRST (so nothing respawns), then let the server child reap
    // itself — a change outside the entry file makes its hot watcher exit with the restart
    // sentinel, and no wrapper remains to restart it.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::write(dir.join("teardown.noe"), "// trigger child exit\n");
    noeta_test_temp::settle_closed(&addr);
    let _ = std::fs::remove_dir_all(&dir);
    // `the edit never hot-swapped in` is a symptom; whatever the watcher said about the edit — a
    // failed re-check, a swap it declined — is in the log, and it goes in the panic beside it.
    outcome.unwrap_or_else(|e| panic!("{}", log.explain(format!("hot swap round trip: {e}"))));
}

/// The same round trip for an app shaped like a real project: a **package** (`noeta.toml`) whose
/// entry imports a sibling module. A module's path derives from its file, so the running module
/// binds the handler as `hotpkg.main.fetch` — and a fragment built from a raw *parse* of the entry
/// carried a plain `fetch`, installing into a fresh global slot: the watcher printed
/// `[hot] swapped: fetch` while the live server went on serving the old body, forever. The swap has
/// to go through the linker, which is what this pins.
#[test]
#[ignore = "spawns the CLI, binds a real socket, and writes real files; run explicitly"]
fn a_hot_swap_lands_inside_a_package_where_the_entry_is_qualified() {
    let dir = noeta_test_temp::TempDir::new("hot-pkg");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("noeta.toml"),
        "[package]\nname = \"local/hotpkg\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/greet.noe"),
        "pub fn greet(): string {\n    return \"hello\"\n}\n",
    )
    .unwrap();
    let app = dir.join("src/main.noe");
    let v = |tag: &str| {
        format!(
            "use std.http.server\n\
             use std.http.{{Request, Response}}\n\
             use std.reactive.{{signal}}\n\
             use hotpkg.greet.greet\n\n\
             count = signal(0)\n\n\
             fn fetch(req: Request) use (count): Response {{\n\
             \x20   count.set(count.get() + 1)\n\
             \x20   return server.response(200, \"${{greet()}} {tag} hits=${{count.get()}}\")\n\
             }}\n"
        )
    };
    std::fs::write(&app, v("v1")).unwrap();

    let port = noeta_test_temp::free_port();
    let log = noeta_test_temp::ServerLog::new("hot-pkg");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args([
                    "serve",
                    "--watch",
                    app.to_str().unwrap(),
                    "--port",
                    &port.to_string(),
                ])
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        let r1 = get(&addr)?;
        if r1 != "hello v1 hits=1" {
            return Err(format!("unexpected v1 response: {r1:?}"));
        }
        std::fs::write(&app, v("v2")).map_err(|e| e.to_string())?;
        let mut hits_seen = 1;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            let r = get(&addr)?;
            let (tag, hits) = r
                .split_once(" hits=")
                .ok_or_else(|| format!("malformed response: {r:?}"))?;
            let hits: i64 = hits.parse().map_err(|_| format!("malformed hits: {r:?}"))?;
            // A restart would reset the counter — this must be a swap, not a restart in disguise.
            if hits != hits_seen + 1 {
                return Err(format!(
                    "counter did not survive: expected {} got {r:?}",
                    hits_seen + 1
                ));
            }
            hits_seen = hits;
            if tag == "hello v2" {
                return Ok(());
            }
        }
        Err(
            "the edit never hot-swapped in — the package's qualified handler was not rebound"
                .to_string(),
        )
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::write(dir.join("src/teardown.noe"), "// trigger child exit\n");
    noeta_test_temp::settle_closed(&addr);
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| {
        panic!(
            "{}",
            log.explain(format!("hot swap round trip in a package: {e}"))
        )
    });
}
