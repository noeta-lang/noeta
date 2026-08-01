//! The perf ratchet's own guard: does it still gate anything?
//!
//! `scripts/perf-ratchet.sh` is the instructions-retired ratchet (startup, the interpreter
//! dispatch loop, a map workload, tier-1 coverage). It needs `perf` and a release binary, so it
//! cannot run inside `cargo test` — which means every way it could quietly stop measuring is
//! invisible to the normal suite. That is not a hypothetical failure mode in this repo: a
//! predecessor regression gate reported success on a run that measured nothing
//! (`fix(bench): a regression gate that measured nothing must not report success`, c619853bd),
//! and the whole reason the ratchet exists is that ~1,800 commits landed a 2x startup regression
//! with nothing watching.
//!
//! So this test watches the watcher, using only the filesystem:
//!
//!   * every row the script declares has a fixture, and every fixture belongs to a row — a row
//!     whose fixture vanished, or a fixture nothing measures, is a gate with a hole in it;
//!   * every fixture sits ALONE in its directory, because an entry's siblings are linked as its
//!     project and a second `.noe` file beside one would silently add its compile to that row's
//!     number (the trap that once inflated a startup figure by 7x);
//!   * the baseline has a numeric entry for every row and records the machine it came from;
//!   * `scripts/gate.sh` still invokes the ratchet at all.
//!
//! The script's `ROWS` array is the single source of truth for what is measured; this test reads
//! it rather than restating it, so the two cannot drift into disagreement.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ratchet_script() -> PathBuf {
    repo_root().join("scripts/perf-ratchet.sh")
}

fn fixtures_root() -> PathBuf {
    repo_root().join("tests/perf/fixtures")
}

/// One entry of the script's `ROWS` array: `id|tolerance|tier-1|expected stdout|argv`.
struct Row {
    id: String,
    tolerance: f64,
    tier1: String,
    argv: String,
}

/// Parse the `ROWS=( … )` array out of the shell script. Deliberately not a re-listing of the
/// rows: a test that hard-codes them would pass on the day someone deletes one.
fn rows() -> Vec<Row> {
    let src = fs::read_to_string(ratchet_script()).expect("scripts/perf-ratchet.sh is missing");
    let body = src
        .split_once("\nROWS=(")
        .expect("scripts/perf-ratchet.sh no longer declares a ROWS=( … ) array — if the rows moved, move this test with them")
        .1
        .split_once("\n)")
        .expect("the ROWS=( … ) array is not closed by a line starting with `)`")
        .0;

    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some(spec) = line.strip_prefix('"').and_then(|l| l.strip_suffix('"')) else {
            continue;
        };
        let f: Vec<&str> = spec.split('|').collect();
        assert_eq!(
            f.len(),
            5,
            "row `{spec}` does not have the 5 fields \
             `id|tolerance|tier-1|expected stdout|argv`"
        );
        out.push(Row {
            id: f[0].to_string(),
            tolerance: f[1]
                .parse()
                .unwrap_or_else(|_| panic!("row `{}` has a non-numeric tolerance `{}`", f[0], f[1])),
            tier1: f[2].to_string(),
            argv: f[4].to_string(),
        });
    }
    assert!(
        !out.is_empty(),
        "the ratchet declares no rows at all — it would report success having measured nothing, \
         which is the exact defect this gate was written to replace"
    );
    out
}

#[test]
fn every_row_is_measurable_and_every_fixture_is_measured() {
    let rows = rows();
    let root = fixtures_root();

    let mut declared: BTreeSet<String> = BTreeSet::new();
    for row in &rows {
        // A row whose argv has no `@` needs no fixture (`--version` measures the binary itself).
        if !row.argv.split_whitespace().any(|a| a == "@") {
            continue;
        }
        declared.insert(row.id.clone());
        let fixture = root.join(&row.id).join(format!("{}.noe", row.id));
        assert!(
            fixture.is_file(),
            "row `{}` measures a fixture that does not exist: {}\n\
             The ratchet would exit 2 (cannot measure) on every run. Restore the fixture, or \
             drop the row from ROWS in scripts/perf-ratchet.sh.",
            row.id,
            fixture.display()
        );
    }

    // …and nothing lying around unmeasured. An orphan fixture is a row someone deleted while
    // believing it still ran.
    let mut present: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir(&root).expect("tests/perf/fixtures is missing") {
        let entry = entry.expect("unreadable entry under tests/perf/fixtures");
        if entry.path().is_dir() {
            present.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    assert_eq!(
        present, declared,
        "tests/perf/fixtures and the ROWS array in scripts/perf-ratchet.sh disagree.\n\
         Only in fixtures/ (measured by nothing): {:?}\n\
         Only in ROWS (nothing to measure): {:?}",
        present.difference(&declared).collect::<Vec<_>>(),
        declared.difference(&present).collect::<Vec<_>>(),
    );
}

#[test]
fn each_fixture_is_alone_in_its_directory() {
    // The sibling-linking trap: an entry's siblings are linked as its project, so a `.noe` file
    // beside a fixture is compiled as part of it and silently added to that row's instruction
    // count. It does not fail, it does not warn — the number just quietly starts measuring
    // something else, which is how a first-pass startup analysis once came out 7x wrong.
    let root = fixtures_root();
    for entry in fs::read_dir(&root).expect("tests/perf/fixtures is missing") {
        let dir = entry.expect("unreadable entry under tests/perf/fixtures").path();
        if !dir.is_dir() {
            continue;
        }
        let noe: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("unreadable fixture directory")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "noe"))
            .collect();
        assert_eq!(
            noe.len(),
            1,
            "{} holds {} .noe files; a perf fixture must be alone in its directory, because the \
             entry's siblings are linked as its project and would be compiled into its \
             measurement: {:?}",
            dir.display(),
            noe.len(),
            noe,
        );
    }
}

#[test]
fn the_baseline_covers_every_row_with_a_real_number() {
    let rows = rows();
    let baseline = repo_root().join("tests/perf/baseline.txt");
    let text = fs::read_to_string(&baseline).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nWithout a baseline the ratchet has nothing to compare against. \
             Record one: scripts/perf-ratchet.sh --record",
            baseline.display()
        )
    });

    assert!(
        text.lines().any(|l| l.starts_with("machine ")),
        "{} has no `machine` line. Instruction counts are not portable across microarchitectures, \
         libc versions or rustc versions, so without it the gate cannot tell a foreign baseline \
         (which it must refuse to judge) from a real regression.",
        baseline.display()
    );

    for row in &rows {
        let prefix = format!("row {} ", row.id);
        let line = text
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| {
                panic!(
                    "{} has no entry for row `{}`. That row would be measured and then compared \
                     against nothing — the ratchet exits 2 rather than passing, but the row is \
                     ungated until someone runs: scripts/perf-ratchet.sh --record",
                    baseline.display(),
                    row.id
                )
            });
        let value: u64 = line
            .split_whitespace()
            .nth(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("baseline line `{line}` has no instruction count"));
        assert!(
            value > 0,
            "row `{}` is baselined at 0 instructions. A zero baseline makes every comparison \
             meaningless and every regression invisible.",
            row.id
        );
    }
}

#[test]
fn tolerances_and_tier_expectations_are_sane() {
    for row in rows() {
        assert!(
            row.tolerance > 0.0 && row.tolerance < 7.0,
            "row `{}` has a {}% tolerance. It must be positive (a zero band flakes on every run) \
             and below 7% — 7% is the size of the interpreter regression this ratchet exists to \
             catch, so a band that wide would have let it through.",
            row.id,
            row.tolerance
        );
        assert!(
            matches!(row.tier1.as_str(), "n/a" | "native" | "declined"),
            "row `{}` declares tier-1 expectation `{}`; the script only understands \
             `n/a`, `native` and `declined`, and an unrecognized value silently never matches.",
            row.id,
            row.tier1
        );
    }
}

#[test]
fn the_merge_gate_still_runs_the_ratchet() {
    // A ratchet nothing invokes is a file, not a gate. `scripts/gate.sh` is where the merge gate
    // is defined, so it has to name this script; if the wiring is ever removed, that should be a
    // failing test rather than a silence.
    let gate = fs::read_to_string(repo_root().join("scripts/gate.sh")).expect("scripts/gate.sh");
    assert!(
        gate.contains("perf-ratchet.sh"),
        "scripts/gate.sh no longer invokes scripts/perf-ratchet.sh — the perf ratchet is not \
         part of the merge gate any more, so a regression it would catch now reaches main."
    );
    assert!(
        gate.contains("--preflight"),
        "scripts/gate.sh invokes the ratchet without --preflight. That probe is what turns \
         `perf is unavailable here` into a visible SKIP naming the reason, instead of a FAIL \
         nobody can act on — or, worse, a pass."
    );
}

#[test]
fn the_script_is_executable() {
    let script = ratchet_script();
    assert!(script.is_file(), "{} is missing", script.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script).expect("stat").permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "{} is not executable (mode {mode:o}); `git update-index --chmod=+x` it",
            script.display()
        );
    }
    let _: &Path = script.as_path();
}
