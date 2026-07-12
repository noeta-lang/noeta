//! Impact-filtered test watch, end to end (server-hmr W3): under `noeta test --watch`, editing a
//! leaf function reruns exactly the tests that (transitively) call it; an inert edit reruns
//! nothing. `#[ignore]` (real processes, real fs events):
//! `cargo test -p noeta-cli --test impact_watch -- --ignored`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn app(leaf_ret: &str) -> String {
    format!(
        "fn leaf(): int {{ return {leaf_ret}; }}\n\
         fn mid(): int {{ return leaf(); }}\n\
         fn other(): int {{ return 2; }}\n\
         @test fn t_mid(): void {{ assert(mid() == {leaf_ret}); }}\n\
         @test fn t_other(): void {{ assert(other() == 2); }}\n"
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
    let dir = std::env::temp_dir().join(format!("noeta-impact-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
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

        // A top-level change: the valve degrades to a full rerun, with the reason.
        std::fs::write(&app_path, format!("{}echo mid()\n", app("0 + 1")))
            .map_err(|e| e.to_string())?;
        wait_for(
            &stderr,
            err_at,
            "rerunning everything: top-level statements changed",
        )?;
        wait_for(&stdout, out_at, "running 2 tests")?;
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.expect("impact-filtered watch round trip");
}
