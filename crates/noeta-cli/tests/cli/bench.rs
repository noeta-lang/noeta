//! `noeta bench`: the `@bench` runner.

use crate::support::*;

// --- measurable fixtures ------------------------------------------------------------
//
// Everything below that saves or compares a baseline needs a **measurement**, not merely a run.
// `noeta bench` estimates per-iteration cost by subtracting two timed points, `(t(2N) − t(N)) / N`;
// when the body is cheap relative to the run-to-run jitter of the fixed overhead, that subtraction
// lands at or below zero and the tool reports *no measurement* — correctly, and loudly. A test that
// then asserts on a delta has nothing to assert on.
//
// That is what made the whole `bench::` family flaky: it passed alone and failed inside the full
// suite, on an arbitrary member each time. The old body — `work(500)` at 2000 iterations — measures
// about **10 ms** of work per point, which is close enough to the noise floor that its worst ambient
// sample was already a third of its median.
//
// **Sizing the body is not, on its own, the fix, and measuring said so.** On this box (20 cores),
// release binary, 40 samples per body, under 3× oversubscription (24 spinning CPU burners on top of
// an ambient load of ~15):
//
// | body           | work per point | unresolved | wall per measurement |
// |----------------|---------------:|-----------:|---------------------:|
// | `work(2500)`   |         ~50 ms |       2/40 |                4.2 s |
// | `work(5000)`   |        ~100 ms |       3/40 |                8.4 s |
//
// Doubling the work doubled the wall time and moved the failure rate not at all — the noise is
// *multiplicative*, so a bigger body scales the signal and the noise together. What fixed it is in
// the tool, where it belongs: `noeta bench` now retries a two-point subtraction that does not
// resolve (`BENCH_MEASUREMENT_ATTEMPTS`), because such a subtraction is a spoiled sample rather than
// a fact about the program.
//
// So the fixture is sized only for what sizing *does* buy — clearing the additive floor an
// idle machine still has — and no further, since past that point every extra millisecond is
// wall time for nothing. Ambient medians, 15 samples: `work(500)` 5.4 µs/iter with samples down to
// 1.9 µs (a 3× spread, right at the floor); `work(2000)` 21 µs/iter with samples down to 13 µs (a
// 1.6× spread, clear of it). Hence ~42 ms per point, about 0.5 s per `noeta bench` invocation on an
// idle box (a measurement is six runs: two points, minimum of three each, the far point double).

/// Per-iteration work in a measured fixture — an `n`-trip integer sum. See the table above.
const MEASURABLE_WORK: u64 = 2_000;

/// The iteration count a measured fixture runs at.
const MEASURABLE_ITERS: u64 = 2_000;

/// The `work(n)` helper every measured fixture shares.
const BENCH_WORK_FN: &str = "fn work(n: int): int {\n\
                             mut t = 0\n\
                             for i in 0..n { t = t + i }\n\
                             return t\n\
                             }\n";

/// One `@bench` fn over [`BENCH_WORK_FN`], sized so its per-iteration cost resolves under load.
fn measurable_bench(name: &str) -> String {
    format!(
        "@bench(iterations: {MEASURABLE_ITERS}) fn {name}(): void {{ work({MEASURABLE_WORK}) }}\n"
    )
}

/// A whole one-file program: the shared `work` helper plus one measurable benchmark per name.
fn measurable_program(names: &[&str]) -> String {
    let mut src = BENCH_WORK_FN.to_string();
    for name in names {
        src.push_str(&measurable_bench(name));
    }
    src
}

// --- `bench` (the `@bench` runner) -------------------------------------------------

#[test]
fn bench_runs_and_reports_each_benchmark() {
    // `lang bench` discovers `@bench` blocks (block + annotation form), measures each, and reports
    // a per-iteration line. Timings are non-deterministic, so only the structure is asserted. The
    // program's own top-level `echo` does not run (the runner runs benches, not the file). A small
    // iteration count keeps the test fast.
    let file = temp_program(
        "bench_ok",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         echo \"main must not run\"\n\
         @bench(iterations: 5) fn small(): void { work(10) }\n\
         @bench(iterations: 5) { fn blocked(): void { work(10) } }\n",
    );
    lang().arg("bench").arg(&file).assert().success().stdout(
        predicate::str::contains("running 2 benchmarks")
            .and(predicate::str::contains("small"))
            .and(predicate::str::contains("blocked"))
            .and(predicate::str::contains("/iter"))
            .and(predicate::str::contains("2 ran, 0 failed, 2 total"))
            .and(predicate::str::contains("main must not run").not()),
    );
}

#[test]
fn bench_positional_iterations_arg_is_read() {
    // A positional `@bench(N)` sets the iteration count, the same as named `@bench(iterations: N)`
    // (name-based dispatch unlocked positional tier args, bound through the shared schema).
    let file = temp_program(
        "bench_positional",
        "fn work(n: int): int { return n }\n@bench(4) fn small(): void { work(1) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("small").and(predicate::str::contains("(4 iterations)")));
}

#[test]
fn bench_name_filter_and_json_report() {
    // `--name` runs exactly the named bench; `--json` reports one machine-readable object with
    // per-bench fields (the editor/CI seam, mirroring `noeta test --json`).
    let file = temp_program(
        "bench_ux",
        "fn work(n: int): int { return n }\n\
         @bench(iterations: 4) {\n\
             fn fast(): void { work(1) }\n\
             fn slow(): void { work(2) }\n\
         }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .arg("--name")
        .arg("fast")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("running 1 benchmark")
                .and(predicate::str::contains("fast"))
                .and(predicate::str::contains("slow").not()),
        );
    let out = lang()
        .arg("bench")
        .arg(&file)
        .arg("--name")
        .arg("fast")
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(json["total"], 1);
    assert_eq!(json["failed"], 0);
    assert_eq!(json["benches"][0]["name"], "fast");
    assert_eq!(json["benches"][0]["iterations"], 4);
    assert!(json["benches"][0]["perIterNs"].is_f64());
}

#[test]
fn bench_calibrates_without_an_iteration_count() {
    // No `--iterations`, no `#[Bench]`: the count is calibrated (grown until a run meets the
    // time target), so the report shows a real count and a real measurement.
    let file = temp_program(
        "bench_calibrate",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench fn body(): void { work(100) }\n",
    );
    let out = lang()
        .arg("bench")
        .arg(&file)
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let iters = json["benches"][0]["iterations"].as_u64().expect("count");
    assert!(iters >= 64, "calibration must grow past the seed: {iters}");
}

#[test]
fn bench_baseline_saves_and_compares() {
    // `--save-baseline` persists a run (per entry file, in the cache dir); `--baseline` diffs
    // against it — the human report gains a delta, the JSON a `baselineDeltaPct`.
    // The body carries the measured margin (see `measurable_program`): a zero has no defined delta,
    // and `--save-baseline` refuses to persist one.
    let file = temp_program("bench_baseline", &measurable_program(&["b"]));
    // Persisting a baseline *is* exercising the cache, so this test owns its cache dir rather than
    // sharing the per-target one (`support::lang`'s convention). Isolation hardening: this test was
    // seen once to report no baseline comparison at all under a fully parallel suite, and did not
    // reproduce — removing the shared-directory variable rules that class out rather than leaving a
    // rare CI red no one can reproduce.
    //
    // That sighting is now understood, and it was not isolation: under enough load the two-point
    // subtraction does not resolve, and `--save-baseline` used to persist the clamped `0.0`, after
    // which `--baseline` skipped the delta in silence and this assertion failed with no explanation.
    // The product no longer does that — the save is refused, with the reason on stderr — so if this
    // test goes red under load again it now says why rather than pointing at a missing substring.
    // The remaining half of that story was the fixture, not the product: refusing to save an
    // unresolved measurement turns a silent wrong answer into a red test, and only a body with real
    // margin (above) stops the machine's load from deciding whether this test passes.
    let cache_dir = PathBuf::from(concat!(
        env!("CARGO_TARGET_TMPDIR"),
        "/bench-baseline-cache"
    ));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let bench = || {
        let mut cmd = lang();
        cmd.env("NOETA_CACHE_DIR", &cache_dir);
        cmd
    };
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--save-baseline")
        .arg("cli-test")
        .assert()
        .success();
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .assert()
        .success()
        .stdout(predicate::str::contains("% vs cli-test"));
    let out = bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert!(json["benches"][0]["baselineDeltaPct"].is_f64());
    // An unknown baseline is a clear error.
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("nope")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no baseline `nope`"));
}

#[test]
fn bench_baseline_says_when_it_cannot_compare() {
    // `--baseline <name>` is a request for a comparison, so every way of not producing one has to be
    // visible. The delta used to be dropped in silence whenever the baseline had no usable entry —
    // which is how a baseline of `0.0` (persisted by the old `.max(0.0)` clamp) made every later
    // comparison vacuous without anything saying so. Here the silent case is the ordinary one: a
    // benchmark added after the baseline was saved.
    // Both benches carry the measured margin (see `measurable_program`). `--save-baseline` *refuses*
    // an unresolved measurement, so a `kept` too cheap to resolve fails this test on the setup step
    // rather than on what it is testing; and an `added` too cheap to resolve reports "this run
    // measured nothing" instead of the missing-entry note this pins.
    let one = measurable_program(&["kept"]);
    let dir = temp_dir("bench_no_entry", &[("b.noe", &one)]);
    let file = dir.join("b.noe");
    // Owns its cache dir for the same reason as `bench_baseline_saves_and_compares` above.
    let cache_dir = dir.join("cache");
    let bench = || {
        let mut cmd = lang();
        cmd.env("NOETA_CACHE_DIR", &cache_dir);
        cmd
    };
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--save-baseline")
        .arg("cli-test")
        .assert()
        .success();

    // A second benchmark the baseline knows nothing about.
    std::fs::write(&file, format!("{one}{}", measurable_bench("added")))
        .expect("write the two-bench program");
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .assert()
        .success()
        .stdout(
            // The known bench still compares; the new one says why it does not.
            predicate::str::contains("% vs cli-test").and(predicate::str::contains(
                "no comparison vs cli-test: this baseline has no entry for this benchmark",
            )),
        );
    // And the `--json` seam carries the same fact, so a delta consumer can tell "unchanged" from
    // "never compared".
    let out = bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("cli-test")
        .arg("--json")
        .assert()
        .success();
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    let added = json["benches"]
        .as_array()
        .expect("benches")
        .iter()
        .find(|b| b["name"] == "added")
        .expect("the added bench");
    assert!(added["baselineDeltaPct"].is_null());
    assert_eq!(
        added["baselineNote"],
        serde_json::json!("this baseline has no entry for this benchmark")
    );
}

#[test]
fn bench_max_regress_gates_ci() {
    // The CI gate: an absurdly permissive limit passes; an impossible limit (any measurement
    // "regresses" past -1000%) fails with the offending bench named on stderr.
    // The body carries the measured margin (see `measurable_program`). This is the test that named
    // the whole flake: with a cheap body the impossible limit **passed** under load, because a run
    // that measured nothing has no delta, and no delta cannot regress. The gate now exits 2 rather
    // than 0 in that state, so a regression here is a red test either way — but the fixture is what
    // keeps this assertion about the gate instead of about the machine's load.
    let file = temp_program("bench_gate", &measurable_program(&["b"]));
    // Owns its cache dir for the same reason as `bench_baseline_saves_and_compares` above.
    let cache_dir = PathBuf::from(concat!(env!("CARGO_TARGET_TMPDIR"), "/bench-gate-cache"));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let bench = || {
        let mut cmd = lang();
        cmd.env("NOETA_CACHE_DIR", &cache_dir);
        cmd
    };
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--save-baseline")
        .arg("gate")
        .assert()
        .success();
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("gate")
        .arg("--max-regress")
        .arg("100000")
        .assert()
        .success();
    bench()
        .arg("bench")
        .arg(&file)
        .arg("--baseline")
        .arg("gate")
        .arg("--max-regress")
        .arg("-1000")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("regressed"));
}

#[test]
fn bench_invalid_arg_is_a_construction_error() {
    // Tier directive args construct the tier's config attribute (`@bench(iterations: true)` ⇒
    // `#[Bench(iterations: true)]`), so a wrong-typed knob is rejected by the ordinary attribute
    // construction gate (E0007, `bool` not assignable to `iterations: int`) — reported up front
    // rather than silently ignored.
    let file = temp_program(
        "bench_bad_arg",
        "@bench(iterations: true) fn b(): void { return }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007").and(predicate::str::contains("iterations")));
}

#[test]
fn bench_per_fn_attribute_overrides_block_arg() {
    // The block's `@bench(iterations: N)` is distribution sugar; a fn carrying its own
    // `#[Bench(…)]` keeps it — the per-fn knob wins.
    let file = temp_program(
        "bench_override",
        "use std.bench.Bench\n\
         fn work(n: int): int { return n }\n\
         @bench(iterations: 4) {\n\
             fn inherits(): void { work(1) }\n\
             #[Bench(iterations: 2)]\n\
             fn overrides(): void { work(1) }\n\
         }\n",
    );
    lang().arg("bench").arg(&file).assert().success().stdout(
        predicate::str::contains("inherits")
            .and(predicate::str::contains("(4 iterations)"))
            .and(predicate::str::contains("overrides"))
            .and(predicate::str::contains("(2 iterations)")),
    );
}

#[test]
fn bench_no_benches_is_success() {
    let file = temp_program("bench_none", "echo \"hi\"\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("no benchmarks found"));
}

#[test]
fn bench_failing_body_is_reported() {
    // A `@bench` whose body aborts (a false `assert`) is a measurement failure, not a crash: the
    // bench is reported FAILED and the process exits non-zero.
    let file = temp_program(
        "bench_fail",
        "@bench(iterations: 2) fn boom(): void { assert(false) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("boom")
                .and(predicate::str::contains("FAILED"))
                .and(predicate::str::contains("1 total")),
        );
}

#[test]
fn bench_unknown_tier_is_e0036() {
    let file = temp_program("bench_badtier", "@bnch { fn x(): void { assert(true) } }\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- `noeta bench <DIR>` (dev-story sweep): a project's benchmarks, not one file's ---

#[test]
fn bench_on_a_directory_measures_every_files_benches() {
    // Same gap `noeta test` had: an entry links a sibling's declarations but never its `@bench`
    // blocks, and a directory argument was a raw `Is a directory (os error 21)` — so a project's
    // benchmarks could not all be run. Outcomes are labelled with the file they came from.
    let dir = temp_dir(
        "bench_dir_all_files",
        &[
            (
                "src/util.noe",
                "namespace Proj.Util;\n\
                 pub fn double(n: int): int { return n * 2; }\n\
                 @bench(iterations: 5)\n\
                 fn util_bench(): void { double(21); }\n",
            ),
            (
                "src/main.noe",
                "use Proj.Util.double\n\
                 echo double(21);\n\
                 @bench(iterations: 5)\n\
                 fn entry_bench(): void { double(1); }\n",
            ),
        ],
    );
    lang().arg("bench").arg(&dir).assert().success().stdout(
        predicate::str::contains("src/util.noe::util_bench")
            .and(predicate::str::contains("src/main.noe::entry_bench"))
            .and(predicate::str::contains("2 ran, 0 failed, 2 total")),
    );
    // The entry alone still measures only the entry — the single-file contract is unchanged.
    lang()
        .arg("bench")
        .arg(dir.join("src/main.noe"))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("1 ran, 0 failed, 1 total")
                .and(predicate::str::contains("util_bench").not()),
        );
}

#[test]
fn bench_directory_baselines_stay_keyed_per_entry_file() {
    // A baseline is keyed by its entry file and by the bare fn name, so a directory run writes
    // exactly the files a per-file run writes — and a later single-file run diffs against it.
    // The body carries the measured margin (see `measurable_program`), exactly as in
    // `bench_baseline_saves_and_compares`: a zero baseline has no defined delta, and the save that
    // opens this test refuses to persist one.
    let program = measurable_program(&["only_bench"]);
    let dir = temp_dir("bench_dir_baseline", &[("src/main.noe", &program)]);
    // Persisting a baseline *is* exercising the cache, so this test owns its cache dir.
    let cache = PathBuf::from(concat!(
        env!("CARGO_TARGET_TMPDIR"),
        "/bench-dir-baseline-cache"
    ));
    let _ = std::fs::remove_dir_all(&cache);
    lang()
        .env("NOETA_CACHE_DIR", &cache)
        .arg("bench")
        .arg(&dir)
        .args(["--save-baseline", "gate"])
        .assert()
        .success();
    lang()
        .env("NOETA_CACHE_DIR", &cache)
        .arg("bench")
        .arg(dir.join("src/main.noe"))
        .args(["--baseline", "gate"])
        // `% vs gate`, not `vs gate`: the "no comparison vs gate: …" note contains the shorter
        // substring, so the loose form passed on exactly the runs this test exists to catch.
        .assert()
        .success()
        .stdout(predicate::str::contains("% vs gate"));
}

#[test]
fn bench_max_regress_cannot_pass_what_it_could_not_compare() {
    // A gate that could not measure must not pass. `--max-regress` used to exit `0` whenever a bench
    // produced no delta — no measurement, no comparison, nothing above the limit, green — which is
    // byte-identical by exit code to "measured, and fine". A gate is read by exit code precisely so
    // nobody has to read stdout, so the inconclusive state gets its own code (`2`, this command's
    // existing "could not do what you asked").
    //
    // The uncomparable state here is the deterministic one — a benchmark the baseline has no entry
    // for, added since it was saved — rather than a body deliberately too cheap to time, which is the
    // same verdict reached by a route that depends on the machine's load.
    let one = measurable_program(&["kept"]);
    let dir = temp_dir("bench_gate_ungated", &[("b.noe", &one)]);
    let file = dir.join("b.noe");
    let cache_dir = dir.join("cache");
    let bench = || {
        let mut cmd = lang();
        cmd.env("NOETA_CACHE_DIR", &cache_dir);
        cmd
    };
    bench()
        .arg("bench")
        .arg(&file)
        .args(["--save-baseline", "gate"])
        .assert()
        .success();
    // The saved bench alone still gates, and passes: a comparison happened.
    bench()
        .arg("bench")
        .arg(&file)
        .args(["--baseline", "gate", "--max-regress", "100000"])
        .assert()
        .success();

    // Add a benchmark the baseline knows nothing about. The limit is still absurdly permissive, so
    // nothing "regressed" — and that is the point: the gate cannot vouch for `added` at all.
    std::fs::write(&file, format!("{one}{}", measurable_bench("added")))
        .expect("write the two-bench program");
    bench()
        .arg("bench")
        .arg(&file)
        .args(["--baseline", "gate", "--max-regress", "100000"])
        .assert()
        .failure()
        .code(2)
        .stderr(
            predicate::str::contains("`added` was not compared")
                .and(predicate::str::contains("could not judge it"))
                .and(predicate::str::contains("inconclusive"))
                // `kept` compared fine; only the bench that did not is named.
                .and(predicate::str::contains("`kept` was not compared").not()),
        );
    // The `--json` seam carries the same count, for a consumer reading the report rather than `$?`.
    let out = bench()
        .arg("bench")
        .arg(&file)
        .args(["--baseline", "gate", "--max-regress", "100000", "--json"])
        .assert()
        .failure()
        .code(2);
    let json: serde_json::Value =
        serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(json["ungated"], 1);
    assert_eq!(json["regressed"], 0);
    assert_eq!(json["failed"], 0);

    // Without `--max-regress` the same run is a *report*, not a gate: it prints why the comparison
    // did not happen and exits 0. The exit code changes only where the exit code is the product.
    bench()
        .arg("bench")
        .arg(&file)
        .args(["--baseline", "gate"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no comparison vs gate: this baseline has no entry for this benchmark",
        ));
}
