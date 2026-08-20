//! **The async corpus must be true on a real host, not only under the sandbox clock.**
//!
//! Every conformance oracle except the linked `--native` AOT differential runs in-process on
//! `SandboxHost` + `SandboxExecutor`, where time is logical: `advance` *jumps* the clock to exactly
//! the next timer deadline, so `sleep(1)` and `sleep(2)` can never come due at the same poll. A
//! case's `// expect:` header is therefore only ever checked against that clock — and a case whose
//! output depends on millisecond gaps can be sandbox-true and real-host-false with nothing in the
//! tree noticing.
//!
//! That is not hypothetical. `async/race.noe` asserted a winner that only held under logical time:
//! on a real host the executor sleeps real time to the earliest deadline and wakes *late*, every
//! deadline the overshoot crossed comes due at the same poll, and the scheduler then resumes those
//! tasks in spawn order. The AOT differential compares two real-host runs to *each other*, never to
//! the header, so it saw nothing until the two sides happened to land differently — as a red CI, on
//! a case whose header had been wrong all along.
//!
//! So this gate asks the question no other one does: run each async case through the **real `noeta`
//! binary** and hold it to its own header. A case that needs real gaps then has to say so in its
//! sleeps, which is what keeps the corpus and the AOT differential checking the same behavior.
//!
//! Cases whose real-host output cannot match a sandbox header are listed in [`HOST_DEPENDENT`] with
//! a reason, and the list is **printed on every run** — an exclusion nobody can see is the failure
//! mode this repo keeps finding. Each row is checked to name a case that still exists, so a row for
//! a deleted or renamed case fails rather than quietly covering nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;

/// Async corpus cases whose real-host output cannot be held to their sandbox header, and why.
///
/// Every row is a **host capability or a real thread** — something the sandbox supplies
/// deterministically and the real host cannot. Deliberately *not* on this list: a case that merely
/// needs its sleeps far enough apart to survive scheduler jitter. That is a defect in the case, and
/// widening the gap is the fix, because a corpus case that only holds under logical time is a case
/// the AOT differential and the sandbox oracles are reading differently.
const HOST_DEPENDENT: &[(&str, &str)] = &[
    (
        "http_async.noe",
        "asserts responses from the sandbox host's built-in HTTP stub; a real host resolves \
         `svc.test` for real and fails",
    ),
    (
        "proc_read_async.noe",
        "spawns a `status` helper the sandbox host stubs; a real host needs it on PATH",
    ),
    (
        "proc_wait_async.noe",
        "spawns a `status` helper the sandbox host stubs; a real host needs it on PATH",
    ),
    (
        "isolate.noe",
        "two isolates are cooperative tasks under the sandbox and genuine OS threads on a real \
         host, so which one prints first is a coin flip (measured 15/10 over 25 loaded runs)",
    ),
    (
        "map_bounded.noe",
        "the two in-flight futures register their deadlines a real millisecond apart, so they need \
         not come due at the same poll; every observed interleave still honors the window this case \
         asserts, but the line order moves (measured 6 of 200 loaded runs)",
    ),
];

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance/async")
}

/// Every `.noe` directly under the async corpus, in a stable order.
fn cases() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .expect("the async conformance corpus is readable")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "noe"))
        .collect();
    paths.sort();
    paths
}

/// The `// expect:` stdout lines and exit code a case declares. The `error` directives are the
/// corpus harness's to render; this gate holds the case to what a *user* sees.
fn expectations(text: &str) -> (Option<Vec<String>>, Option<i32>) {
    let mut stdout: Option<Vec<String>> = None;
    let mut exit = None;
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("// expect: stdout ") {
            stdout
                .get_or_insert_with(Vec::new)
                .push(unquote(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("// expect: exit ") {
            exit = rest.trim().parse().ok();
        }
    }
    (stdout, exit)
}

/// The header's quoted-string form, with the same escapes the corpus harness accepts.
fn unquote(s: &str) -> String {
    let inner = s.trim_matches('"');
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// `noeta run <case>` from a **fresh empty directory**, so a case that writes real files (the
/// sandbox gives it a virtual disk; the real host gives it the actual one) neither reads another
/// case's leftovers nor litters the checkout.
fn run_case(path: &Path) -> std::process::Output {
    let stem = path.file_stem().expect("a case has a file name");
    let cwd = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("real-host")
        .join(stem);
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("a scratch directory for the case");
    Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        // Hermetic startup cache — never touch the developer's real ~/.cache/noeta.
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .current_dir(&cwd)
        .arg("run")
        .arg(path)
        .output()
        .expect("spawn noeta")
}

/// A listed case must still exist: a row naming a deleted or renamed case covers nothing, and would
/// hide the next case that lands with the same problem.
#[test]
fn every_host_dependent_row_names_a_real_case() {
    let stale: Vec<&str> = HOST_DEPENDENT
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !corpus_dir().join(name).exists())
        .collect();
    assert!(
        stale.is_empty(),
        "HOST_DEPENDENT names cases that are not in the async corpus — delete or rename the \
         row(s): {stale:?}"
    );
}

/// **Every unlisted async case produces its `// expect:` output through the real `noeta` binary.**
#[test]
fn async_corpus_holds_on_the_real_host() {
    let cases = cases();
    assert!(
        !cases.is_empty(),
        "the async conformance corpus is empty — this gate would assert nothing"
    );

    eprintln!(
        "real-host corpus gate: {} async case(s), {} excluded as host-dependent:",
        cases.len(),
        HOST_DEPENDENT.len()
    );
    for (name, why) in HOST_DEPENDENT {
        eprintln!("  EXCLUDED {name} — {why}");
    }

    let checked: Vec<&PathBuf> = cases
        .iter()
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            !HOST_DEPENDENT.iter().any(|(listed, _)| *listed == name)
        })
        .collect();

    let judge = |path: &PathBuf| -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        let (expected, expected_exit) = expectations(&text);
        if expected.is_none() && expected_exit.is_none() {
            return None;
        }
        let output = run_case(path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let actual: Vec<String> = stdout.lines().map(str::to_string).collect();
        let mut problems = Vec::new();
        if let Some(expected) = &expected
            && &actual != expected
        {
            problems.push(format!("stdout: expected {expected:?}, got {actual:?}"));
        }
        if let Some(code) = expected_exit
            && output.status.code() != Some(code)
        {
            problems.push(format!(
                "exit: expected {code}, got {:?}",
                output.status.code()
            ));
        }
        if problems.is_empty() {
            return None;
        }
        Some(format!("  {}: {}", path.display(), problems.join("; ")))
    };

    let failures: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = checked
            .iter()
            .map(|path| scope.spawn(move || judge(path)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("case thread"))
            .collect::<Vec<_>>()
    });

    assert!(
        failures.is_empty(),
        "these async corpus cases are true under the sandbox clock and false on a real host — \
         either the case depends on millisecond gaps a real scheduler does not preserve (widen \
         them), or it needs a capability only the sandbox host supplies (add a HOST_DEPENDENT \
         row):\n{}",
        failures.join("\n")
    );
}
