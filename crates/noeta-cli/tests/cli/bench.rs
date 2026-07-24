//! `noeta bench` (object-model slice 6): the `@bench` runner.

use crate::support::*;

// --- `bench` (object-model slice 6: the `@bench` runner) ---------------------------

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
    // Enough per-iteration work that the two-point measurement is reliably non-zero — a zero
    // baseline has no defined delta.
    let file = temp_program(
        "bench_baseline",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench(iterations: 2000) fn b(): void { work(500) }\n",
    );
    // Persisting a baseline *is* exercising the cache, so this test owns its cache dir rather than
    // sharing the per-target one (`support::lang`'s convention). Isolation hardening: this test was
    // seen once to report no baseline comparison at all under a fully parallel suite, and did not
    // reproduce — removing the shared-directory variable rules that class out rather than leaving a
    // rare CI red no one can reproduce.
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
fn bench_max_regress_gates_ci() {
    // The CI gate: an absurdly permissive limit passes; an impossible limit (any measurement
    // "regresses" past -1000%) fails with the offending bench named on stderr.
    let file = temp_program(
        "bench_gate",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         @bench(iterations: 2000) fn b(): void { work(500) }\n",
    );
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
