//! Multi-worker in-process hot reload: under `noeta serve --parallel N --watch`,
//! a source edit **broadcasts** to every worker isolate — each drains the shared swap queue and
//! serves the new code, no restart.
//!
//! `#[ignore]`d for the real port, threads and fs events it needs, and run by name from ci.yml's
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest).
//! By hand: `cargo test -p noeta-cli --test parallel_hot -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

mod common;

/// The served handler, tagged so a response says which version answered it.
///
/// The program is `tests/fixtures/parallel_hot/app.noe`, not a literal here: this test is
/// `#[ignore]`d, so an inline program would be compiled by no `cargo test` and a language change
/// would rot it in silence. On disk, `tests/fixtures.rs` compiles it on every run. The tag is
/// substituted into the fixture's own `"v1"`, which keeps the file on disk a valid program rather
/// than a template with a hole in it.
fn app(tag: &str) -> String {
    common::fixture_with("parallel_hot/app", &[("\"v1\"", &format!("\"{tag}\""))])
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
    // Three workers behind a watch wrapper, all writing to one file this test can quote — rather
    // than to `/dev/null`, which is how a lost bind in this very suite came to be reported as a
    // bare `Connection refused` and blamed on the readiness budget (`noeta_test_temp::ServerLog`).
    let log = noeta_test_temp::ServerLog::new("parallel-hot");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args([
                    "serve",
                    "--watch",
                    app_path.to_str().unwrap(),
                    "--port",
                    &port.to_string(),
                    "--parallel",
                    "3",
                ])
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --parallel 3 --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
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
    outcome.unwrap_or_else(|e| {
        panic!(
            "{}",
            log.explain(format!("parallel hot broadcast round trip: {e}"))
        )
    });
}

/// How long an idle swap gets, with **no traffic at all**, before the one request that must already
/// see it. Generous: the watcher debounces 150ms, re-links, checks and diffs, and the run thread
/// then has to be roused and reach a scheduler tick. What this test deliberately does not do is
/// retry that first request — a retry would turn the wake into "the swap landed eventually", which
/// is exactly the behavior the wake exists to beat.
const IDLE: Duration = Duration::from_millis(3000);

/// How many requests each phase makes: enough that **every** worker of the fleet is hit, several
/// times over. It scales with N because the assertion below is now zero-stale rather than a bound,
/// and a worker no request reaches is a worker the assertion never asked about — at a fixed 9, a
/// 5-worker fleet leaves one untouched roughly two rounds in three. Six per worker puts that under
/// a percent, and a loopback request costs microseconds.
fn fan(workers: usize) -> usize {
    6 * workers.max(2)
}

/// What one idle-swap round trip observed.
struct Swap {
    /// The tags served before the edit. All `v1`, in every shape.
    before: Vec<String>,
    /// The tag served by the **single request made after the edit and the idle wait**: the wake's
    /// own assertion for the single worker, made with no retry.
    first_after_idle: String,
    /// How many post-idle responses came back stale (`v1`) before the swap had reached every
    /// consumer. **Zero in every shape** — see the test.
    stale_after_idle: usize,
    /// Whether every consumer settled on the new code within the bound below.
    settled: bool,
}

/// One idle-swap round trip against `noeta serve --watch`, in the fleet (`Some(n)`) or alone
/// (`None`).
fn idle_swap_round_trip(parallel: Option<usize>) -> Result<Swap, String> {
    let fan = fan(parallel.unwrap_or(1));
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
    let log = noeta_test_temp::ServerLog::new("hot-install-idle");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args(&args)
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<Swap, String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        let mut before = Vec::new();
        for _ in 0..fan {
            before.push(get(&addr)?);
        }
        std::fs::write(&app_path, app("v2")).map_err(|e| e.to_string())?;
        // The wake's whole job: the watcher deposits into a server with nothing in
        // flight and rouses it *then*, rather than leaving the swap to be picked up by whichever
        // request happens along next. So: no traffic, then exactly one request.
        std::thread::sleep(IDLE);
        let first_after_idle = get(&addr)?;
        let mut stale_after_idle = usize::from(first_after_idle != "v2");
        // Then settle: every consumer must be serving the new code, not just the one that answered.
        let mut settled = false;
        for _ in 0..40 {
            let mut round = Vec::new();
            for _ in 0..fan {
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
    noeta_test_temp::settle_closed(&addr);
    let _ = std::fs::remove_dir_all(&dir);
    // The error travels up to a `panic!` in the caller, so the server's own words have to travel
    // with it — this helper is the last place that still holds the log.
    outcome.map_err(|e| log.explain(e))
}

/// **The fleet and the single worker are one hot install** (plans/parallel-path-audit.md row 10),
/// and an *idle* server applies a swap **before** its next request rather than during it.
///
/// The two paths used to be two hand-written copies of one nine-step install, free to differ in any
/// step, and they did — the parallel one took a `SourceMap` it had no code to use, because the step
/// that consumes one was never copied across. They are one `HotRig` now, and this is what keeps
/// them one: the same edit against the same program, alone and across fleets of 1, 2, 3 and 5
/// worker isolates, judged by the same assertions. Every step of the install is under it — a
/// dropped mailbox
/// or an unarmed watcher means no swap at all; a dropped `set_wake` means the idle request still
/// serves `v1` and the *next* one serves `v2`, which is precisely the one-request lag the wake was
/// added to remove; a fleet that registered one consumer instead of N loses the swap for the other
/// workers; and a restart-in-disguise cannot happen with the wrapper killed.
///
/// **The assertion is zero stale responses in every shape, and it used to be a bound.** It read
/// `fleet.stale_after_idle < WORKERS`, written when nobody knew what the real number was — and a
/// bound a bug satisfies is how the bug stayed invisible: an idle fleet was applying the swap in one
/// or two workers whatever N was (0 stale at N=1 and 2, 1–2 at N=3, 3–4 at N=5), comfortably under
/// the bound at every N, for as long as the bound was the test. Every shape now has to reach zero,
/// which is a claim a fleet either meets or fails.
///
/// The shapes are N = 1, 2, 3 and 5 plus the bare single worker, because the defect was invisible
/// below three: the drain race it came from needs three simultaneous wakes before anybody loses it
/// (`noeta_vm::HotChannel::drain`). A single worker and a fleet of two agreed on zero throughout.
#[test]
#[ignore = "spawns five CLI servers, binds real sockets and edits real files; run explicitly"]
fn an_idle_swap_reaches_every_worker_before_the_next_request() {
    // `None` is the bare single worker (its program binds its own socket); the rest are fleets over
    // one pre-bound listener. Five spawns, ~4s each.
    const SHAPES: [Option<usize>; 5] = [None, Some(1), Some(2), Some(3), Some(5)];
    let runs: Vec<(String, Swap)> = SHAPES
        .iter()
        .map(|shape| {
            let what = match shape {
                None => "the single worker".to_string(),
                Some(n) => format!("the {n}-worker fleet"),
            };
            let swap =
                idle_swap_round_trip(*shape).unwrap_or_else(|e| panic!("{what}'s round trip: {e}"));
            (what, swap)
        })
        .collect();

    for (what, swap) in &runs {
        let all_v1: Vec<String> =
            std::iter::repeat_n("v1".to_string(), swap.before.len()).collect();
        assert_eq!(swap.before, all_v1, "{what} did not start on v1");
        assert!(
            swap.settled,
            "{what} never settled on the new code — the swap did not reach every consumer"
        );
        // The wake, stated directly and with no retry: the swap was deposited into a server with
        // nothing in flight, so it must be installed BEFORE the next request rather than during it.
        assert_eq!(
            swap.first_after_idle, "v2",
            "{what} served stale code on the very first request after an idle swap — the watcher's \
             deposit did not rouse it (`RealExecutor::set_wake`, server-hmr L3)"
        );
        assert_eq!(
            swap.stale_after_idle, 0,
            "{what} served {} stale response(s) after an idle wake — a worker that was roused, lost \
             the race for the swap queue, and parked again with no second tick to retry on \
             (`noeta_vm::HotChannel::drain`)",
            swap.stale_after_idle
        );
    }
}

/// **An edit made the instant the server answers is hot-swapped, not lost.**
///
/// Arming the watcher used to mean *spawning the thread that arms it*, and `std::thread::spawn`
/// returns before that thread has necessarily run. The boot's next steps are to compile and bind,
/// which for a small program on a loaded box finish first, so the server announced
/// `listening … hot-reloading` while nothing was subscribed. `notify` has no backlog for a watch
/// that did not exist, and the watcher has no second chance to look: an edit in that window raised
/// no event, printed no diagnostic, and was never retried.
///
/// This edits with **no settle at all**, which is the worst case and the one the two tests above
/// brush against, since they serve only a short round of requests first. The assertion is the
/// watcher's own deposit line rather than the served body, because the `--watch` wrapper restarts
/// the process on a change it sees first, and a restart also ends up serving the new code. A restart
/// is not a swap: it drops the signal state a swap preserves, which is the thing hot reload exists
/// for. Only `[hot] swapped generation` tells the two apart.
#[test]
#[ignore = "spawns the CLI across threads and edits real files; run explicitly"]
fn an_edit_made_as_soon_as_the_server_answers_is_still_swapped() {
    let dir = noeta_test_temp::TempDir::new("parallel-hot-arm");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app("v1")).unwrap();

    let port = noeta_test_temp::free_port();
    let log = noeta_test_temp::ServerLog::new("parallel-hot-arm");
    let mut child = log
        .spawn(
            Command::new(env!("CARGO_BIN_EXE_noeta"))
                .args([
                    "serve",
                    "--watch",
                    app_path.to_str().unwrap(),
                    "--port",
                    &port.to_string(),
                    "--parallel",
                    "3",
                ])
                .current_dir(&dir),
        )
        .expect("spawn `noeta serve --parallel 3 --watch`");
    let addr = format!("127.0.0.1:{port}");

    let outcome = (|| -> Result<(), String> {
        noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
        // No settle, no warm-up request: the edit goes in the moment the port answers.
        std::fs::write(&app_path, app("v2")).map_err(|e| e.to_string())?;

        // Generous, because what is being asserted is that the edit is honored AT ALL, never how
        // fast. The watcher has to debounce, re-link, check, diff and compile first, and none of
        // that is this test's subject.
        let mut swapped = false;
        for _ in 0..600 {
            // The watcher's own deposit line, not a worker's install line: this asserts that the
            // edit was SEEN, which is the thing an unarmed watch loses.
            if log.tail().contains("[hot] swapped generation") {
                swapped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !swapped {
            return Err(
                "the edit raised no swap — the server was listening before its watcher was \
                 armed, so the event reached nobody and there is no retry"
                    .to_string(),
            );
        }
        // And it really is serving the new code, from every worker.
        for _ in 0..12 {
            let r = get(&addr)?;
            if r != "v2" {
                return Err(format!("swapped, but a worker still serves {r:?}"));
            }
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| panic!("{}", log.explain(format!("arming race: {e}"))));
}
