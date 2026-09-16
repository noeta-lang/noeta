//! Impact-filtered test watch, end to end: under `noeta test --watch`, editing a
//! leaf function reruns exactly the tests that (transitively) call it; an inert edit reruns
//! nothing.
//!
//! `#[ignore]`d for the real processes and fs events it needs, and run by name from ci.yml's `jit`
//! `scripts/hot-e2e.sh`, which both ci.yml and `scripts/gate.sh` run (`tests/cli/automation.rs` keeps that list honest). By hand:
//! `cargo test -p noeta-cli --test impact_watch -- --ignored`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

/// The single-file app: a leaf, a caller, and one test on each.
///
/// Every program this suite writes is under `tests/fixtures/impact_watch/`, where `tests/fixtures.rs`
/// compiles it on each `cargo test`. Written here as a literal it would be compiled nowhere, since
/// `#[ignore]` means nothing runs it unless someone types `--ignored`. `leaf_ret` replaces the
/// fixture's own `1`, in both the body and the assertion that pins it.
fn app(leaf_ret: &str) -> String {
    app_from("impact_watch/app", leaf_ret)
}

/// The same app with a top-level statement after the declarations, which is the edit the impact
/// valve cannot attribute and answers with a full rerun.
///
/// A second file rather than an appended line, because a top-level statement is structure and the
/// version the test actually writes has to be compiled somewhere.
fn app_with_top_level(leaf_ret: &str) -> String {
    app_from("impact_watch/app_with_top_level", leaf_ret)
}

fn app_from(name: &str, leaf_ret: &str) -> String {
    common::fixture_with(
        name,
        &[
            ("return 1;", &format!("return {leaf_ret};")),
            ("mid() == 1", &format!("mid() == {leaf_ret}")),
        ],
    )
}

/// Spawn a reader thread that appends a stream's bytes into a shared string.
fn tail(stream: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let out = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
            }
            out.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    buf
}

/// Wait until the shared buffer contains `needle` at or after `from`, returning the buffer length
/// afterwards (the next call's `from`).
fn wait_for(buf: &Arc<Mutex<String>>, from: usize, needle: &str) -> Result<usize, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        {
            let s = buf.lock().unwrap();
            if s[from.min(s.len())..].contains(needle) {
                return Ok(s.len());
            }
        }
        if Instant::now() > deadline {
            let s = buf.lock().unwrap();
            return Err(format!(
                "`{needle}` did not appear within 10s; output after {from}:\n{}",
                &s[from.min(s.len())..]
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "spawns the CLI and writes real files; run explicitly"]
fn an_edit_reruns_exactly_the_impacted_tests() {
    let dir = noeta_test_temp::TempDir::new("impact-watch");
    let app_path = dir.join("app.noe");
    std::fs::write(&app_path, app("1")).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["test", "--watch", "app.noe"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `noeta test --watch`");
    let stdout = tail(child.stdout.take().unwrap());
    let stderr = tail(child.stderr.take().unwrap());

    let outcome = (|| -> Result<(), String> {
        // First run: everything (2 tests), then the wrapper parks.
        let out_at = wait_for(&stdout, 0, "running 2 tests")?;
        let err_at = wait_for(&stderr, 0, "waiting for changes")?;

        // A leaf edit: exactly leaf → mid → t_mid impacted; the rerun selects ONE test.
        std::fs::write(&app_path, app("0 + 1")).map_err(|e| e.to_string())?;
        let err_at = wait_for(&stderr, err_at, "impacted: leaf, mid, t_mid")?;
        let out_at = wait_for(&stdout, out_at, "running 1 test")?;
        let err_at = wait_for(&stderr, err_at, "waiting for changes")?;

        // An inert edit (a blank line between declarations): no run at all.
        std::fs::write(&app_path, format!("{}\n\n", app("0 + 1"))).map_err(|e| e.to_string())?;
        let err_at = wait_for(&stderr, err_at, "nothing impacted")?;

        // A top-level change: the valve degrades to a full rerun, with the reason (prefixed
        // by the file it came from — the multi-file engine attributes per member).
        std::fs::write(&app_path, app_with_top_level("0 + 1")).map_err(|e| e.to_string())?;
        wait_for(
            &stderr,
            err_at,
            "rerunning everything: app.noe: top-level statements changed",
        )?;
        wait_for(&stdout, out_at, "running 2 tests")?;
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("impact-filtered watch round trip");
}

/// The imported module, with both bodies substituted into `tests/fixtures/impact_watch/multi/lib.noe`.
fn lib(add_body: &str, stray_body: &str) -> String {
    lib_from("impact_watch/multi/lib", add_body, stray_body)
}

/// The same module with `stray` carrying a parameter, which is the signature change the last step of
/// the multi-file test makes.
///
/// It is a second file rather than a `str::replace` on the first, because a signature is structure:
/// the padded version has to be compiled somewhere, and only a fixture on disk is. It sits in its own
/// directory so that two modules claiming `App.Lib` never share one.
fn lib_with_padded_stray(add_body: &str, stray_body: &str) -> String {
    lib_from("impact_watch/multi_padded/lib", add_body, stray_body)
}

fn lib_from(name: &str, add_body: &str, stray_body: &str) -> String {
    common::fixture_with(
        name,
        &[
            ("return a + b;", &format!("return {add_body};")),
            ("return 9;", &format!("return {stray_body};")),
        ],
    )
}

/// The entry that imports `App.Lib.add`, and the two tests whose impact this suite narrows.
fn app_using_lib() -> String {
    common::fixture("impact_watch/multi/app")
}

#[test]
#[ignore = "spawns the CLI and writes real files; run explicitly"]
fn a_sibling_module_edit_narrows_to_its_caller_tests() {
    // The multi-file impact arc's headline: editing an IMPORTED module reruns exactly the
    // entry tests that transitively reach the change — the pre-salsa engine degraded every
    // non-entry edit to a full rerun.
    let dir = noeta_test_temp::TempDir::new("impact-watch-mf");
    let lib_path = dir.join("lib.noe");
    std::fs::write(&lib_path, lib("a + b", "9")).unwrap();
    std::fs::write(dir.join("app.noe"), app_using_lib()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["test", "--watch", "app.noe"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `noeta test --watch`");
    let stdout = tail(child.stdout.take().unwrap());
    let stderr = tail(child.stderr.take().unwrap());

    let outcome = (|| -> Result<(), String> {
        // First run: both entry tests.
        let out_at = wait_for(&stdout, 0, "running 2 tests")?;
        let err_at = wait_for(&stderr, 0, "waiting for changes")?;

        // Edit the imported lib fn: the closure crosses the module boundary in the linked
        // program's qualified vocabulary and lands on the ONE test that reaches it.
        std::fs::write(&lib_path, lib("b + a", "9")).map_err(|e| e.to_string())?;
        let err_at = wait_for(&stderr, err_at, "impacted: App.Lib.add, compose, t_add")?;
        let out_at = wait_for(&stdout, out_at, "running 1 test")?;
        let err_at = wait_for(&stderr, err_at, "waiting for changes")?;

        // Edit a lib fn OUTSIDE the entry's import closure: impacted, but no test reaches it —
        // the runner's `--name` filter matches nothing and nothing runs.
        std::fs::write(&lib_path, lib("b + a", "10 - 1")).map_err(|e| e.to_string())?;
        let err_at = wait_for(&stderr, err_at, "impacted: App.Lib.stray")?;
        let out_at = wait_for(&stdout, out_at, "no tests matching --name")?;
        let err_at = wait_for(&stderr, err_at, "waiting for changes")?;

        // An inert lib edit (formatting between declarations): no run at all.
        std::fs::write(&lib_path, format!("{}\n\n", lib("b + a", "10 - 1")))
            .map_err(|e| e.to_string())?;
        let err_at = wait_for(&stderr, err_at, "nothing impacted")?;

        // A lib signature change: unattributable — full rerun, reason names the file.
        std::fs::write(&lib_path, lib_with_padded_stray("b + a", "10 - 1"))
            .map_err(|e| e.to_string())?;
        wait_for(&stderr, err_at, "rerunning everything: lib.noe:")?;
        wait_for(&stdout, out_at, "running 2 tests")?;
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("multi-file impact-filtered watch round trip");
}
