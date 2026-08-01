//! Multi-worker in-process hot reload (server-hmr F5): under `noeta serve --parallel N --watch`,
//! a source edit **broadcasts** to every worker isolate — each drains the shared swap queue and
//! serves the new code, no restart.
//!
//! `#[ignore]`d for the real port, threads and fs events it needs, and run by name from ci.yml's
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest).
//! By hand: `cargo test -p noeta-cli --test parallel_hot -- --ignored`.

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
    let dir = noeta_test_temp::TempDir::new("parallel-hot");
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

/// How long an idle swap gets, with **no traffic at all**, before the one request that must already
/// see it. Generous: the watcher debounces 150ms, re-links, checks and diffs, and the run thread
/// then has to be roused and reach a scheduler tick. What this test deliberately does not do is
/// retry that first request — a retry would turn the wake into "the swap landed eventually", which
/// is exactly the behavior the wake exists to beat.
const IDLE: Duration = Duration::from_millis(3000);

/// How many requests each phase makes — enough to fan across a 3-worker fleet several times over.
const FAN: usize = 9;

/// What one idle-swap round trip observed.
struct Swap {
    /// The tags served before the edit. All `v1`, in both shapes.
    before: Vec<String>,
    /// The tag served by the **single request made after the edit and the idle wait**: the wake's
    /// own assertion for the single worker, made with no retry.
    first_after_idle: String,
    /// How many post-idle responses came back stale (`v1`) before the swap had reached every
    /// consumer. Zero for a single worker. See the test for what bounds it in a fleet.
    stale_after_idle: usize,
    /// Whether every consumer settled on the new code within the bound below.
    settled: bool,
}

/// One idle-swap round trip against `noeta serve --watch`, in the fleet (`Some(n)`) or alone
/// (`None`).
fn idle_swap_round_trip(parallel: Option<usize>) -> Result<Swap, String> {
    let dir = noeta_test_temp::TempDir::new("hot-install-idle");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app("v1")).map_err(|e| e.to_string())?;

    // A kernel-assigned port, not a fixed one: a fixed port is shared with every other checkout and
    // every concurrent run of this test on the machine.
    let port = noeta_test_temp::free_port();
    let mut args: Vec<String> = vec![
        "serve".into(),
        "--watch".into(),
        app_path.to_str().unwrap().into(),
        "--port".into(),
        port.to_string(),
    ];
    if let Some(workers) = parallel {
        args.push("--parallel".into());
        args.push(workers.to_string());
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(&args)
        .current_dir(&dir)
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<Swap, String> {
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
        let mut before = Vec::new();
        for _ in 0..FAN {
            before.push(get(&addr)?);
        }
        std::fs::write(&app_path, app("v2")).map_err(|e| e.to_string())?;
        // The wake's whole job (server-hmr L3): the watcher deposits into a server with nothing in
        // flight and rouses it *then*, rather than leaving the swap to be picked up by whichever
        // request happens along next. So: no traffic, then exactly one request.
        std::thread::sleep(IDLE);
        let first_after_idle = get(&addr)?;
        let mut stale_after_idle = usize::from(first_after_idle != "v2");
        // Then settle: every consumer must be serving the new code, not just the one that answered.
        let mut settled = false;
        for _ in 0..40 {
            let mut round = Vec::new();
            for _ in 0..FAN {
                round.push(get(&addr)?);
            }
            let stale = round.iter().filter(|r| *r != "v2").count();
            stale_after_idle += stale;
            if stale == 0 {
                settled = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Swap {
            before,
            first_after_idle,
            stale_after_idle,
            settled,
        })
    })();

    // Teardown: kill the wrapper FIRST (so nothing respawns), then let the server child reap itself
    // — a change outside the entry file makes its hot watcher exit with the restart sentinel, and no
    // wrapper remains to restart it.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::write(dir.join("teardown.noe"), "// trigger child exit\n");
    for _ in 0..40 {
        if TcpStream::connect(&addr).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// **The fleet and the single worker are one hot install** (plans/parallel-path-audit.md row 10),
/// and an *idle* server applies a swap **before** its next request rather than during it.
///
/// The two paths used to be two hand-written copies of one nine-step install, free to differ in any
/// step, and they did — the parallel one took a `SourceMap` it had no code to use, because the step
/// that consumes one was never copied across. They are one `HotRig` now, and this is what keeps
/// them one: the same edit against the same program, once across three worker isolates and once
/// alone, judged by the same assertions. Every step of the install is under it — a dropped mailbox
/// or an unarmed watcher means no swap at all; a dropped `set_wake` means the idle request still
/// serves `v1` and the *next* one serves `v2`, which is precisely the one-request lag the wake was
/// added to remove; a fleet that registered one consumer instead of N loses the swap for the other
/// workers; and a restart-in-disguise cannot happen with the wrapper killed.
///
/// The one asymmetry is measured, not assumed, and is a finding this row records rather than fixes
/// (see `watch::wake_all`). The wake reaches exactly two workers of an idle fleet whatever N is, so
/// the single worker is held to **zero** stale responses after an idle swap and a fleet to *fewer
/// than one per worker*. Both bounds are what a dropped `set_wake` breaks — without it every worker
/// answers exactly one request with pre-swap code, measured 1 of 1 and 3 of 3 — so the mutation
/// fails this test on both shapes rather than on neither.
#[test]
#[ignore = "spawns two CLI servers, binds real sockets and edits real files; run explicitly"]
fn a_fleet_and_a_single_worker_apply_an_idle_swap_identically() {
    const WORKERS: usize = 3;
    let fleet = idle_swap_round_trip(Some(WORKERS)).expect("the fleet's round trip");
    let single = idle_swap_round_trip(None).expect("the single worker's round trip");

    let all_v1: Vec<String> = std::iter::repeat_n("v1".to_string(), FAN).collect();
    for (what, swap) in [("fleet", &fleet), ("single worker", &single)] {
        assert_eq!(swap.before, all_v1, "the {what} did not start on v1");
        assert!(
            swap.settled,
            "the {what} never settled on the new code — the swap did not reach every consumer"
        );
    }
    // The two installs are one install, so they agree on where they started and where they ended.
    assert_eq!(
        fleet.before, single.before,
        "the fleet and the single worker disagreed before any edit"
    );
    // The wake, stated directly and with no retry: the swap was deposited into a server with
    // nothing in flight, so it must be installed BEFORE the next request rather than during it.
    assert_eq!(
        single.first_after_idle, "v2",
        "the single worker served stale code on the very first request after an idle swap — the \
         watcher's deposit did not rouse it (`RealExecutor::set_wake`, server-hmr L3)"
    );
    assert_eq!(
        single.stale_after_idle, 0,
        "the single worker served stale code at some point after an idle wake"
    );
    assert!(
        fleet.stale_after_idle < WORKERS,
        "the fleet served {} stale responses after an idle wake, one for every one of its \
         {WORKERS} workers — that is the un-woken one-request lag on every consumer, i.e. the wake \
         reached none of them",
        fleet.stale_after_idle
    );
}
