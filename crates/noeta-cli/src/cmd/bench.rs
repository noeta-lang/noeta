//! `noeta bench` — measure a program's `@bench` blocks with two-point calibrated runs,
//! and persist/diff machine-local baselines.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use noeta_ast::{AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_runner::compile_real;
use noeta_vm::VmBackend;

use crate::cmd::test::{call_stmt, is_tier_setup};
use crate::context::{Prologue, check_under, tier_prologue};
use crate::output::plural;

/// The fallback iteration count when calibration cannot estimate per-iteration cost (a probe too
/// noisy to subtract). Small, because the runner executes *interpreted* code and measures at both
/// N and 2N (see [`cmd_bench`]).
pub(crate) const DEFAULT_BENCH_ITERATIONS: u64 = 200;

/// Calibration's target wall time for one measurement point, in nanoseconds (~50ms): long enough
/// to dwarf timer noise, short enough that the six runs a two-point/min-of-three measurement
/// takes stay comfortable.
pub(crate) const BENCH_TARGET_POINT_NS: f64 = 50_000_000.0;

/// Calibration's iteration-count ceiling — a nanosecond-scale body would otherwise be sized into
/// millions of synthesized call statements (the harness materializes one `Stmt` per iteration).
pub(crate) const BENCH_MAX_ITERATIONS: u64 = 100_000;

/// One benchmark's result — the human report, the `--json` seam, and the baseline file all read
/// from this.
pub(crate) struct BenchOutcome {
    name: String,
    iterations: u64,
    /// `Some` on success; `None` when the bench failed (see `message`).
    per_iter_ns: Option<f64>,
    message: Option<String>,
    /// Percent change vs the `--baseline` entry, when one exists for this bench.
    baseline_delta_pct: Option<f64>,
}

/// `noeta bench <FILE>` — discover the program's `@bench` blocks (object-model slice 6) and measure
/// each. Unlike `noeta test`, benchmarks run **sequentially** (concurrency would corrupt timings).
/// Each bench's per-iteration cost is estimated by a **two-point** measurement: the fn is invoked N
/// and 2N times in fresh isolates and the per-iteration time is `(t(2N) − t(N)) / N`, which cancels
/// the fixed per-run overhead (runtime startup, global/setup evaluation, IR lowering — all identical
/// between the two runs). N comes from `--iterations`, else the per-bench `@bench(iterations: N)`
/// directive, else **calibration** (a two-run probe sizes N so one point takes
/// [`BENCH_TARGET_POINT_NS`]). `--name` filters (exact fn-name match, repeatable); `--json`
/// reports one machine-readable object; `--save-baseline`/`--baseline` persist and diff runs
/// (per entry file, in the noeta cache — timings are machine-local).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_bench(
    file: &std::path::Path,
    iterations_override: Option<u64>,
    names: &[String],
    json: bool,
    save_baseline: &Option<String>,
    baseline: &Option<String>,
    max_regress: Option<f64>,
    target: &Option<String>,
) -> ExitCode {
    // The shared tier prologue: compose delegation, the `--target` gate, the dep-aware load,
    // provider dispatch (a `bench = "<pkg>"` target hands the tier to that package's runner),
    // activation diagnostics, and the whole-program type check.
    let run = match tier_prologue(file, "bench", target) {
        Prologue::Ran(code) => return code,
        Prologue::Ready(run) => *run,
    };
    let activated = &run.activated;

    // `--name` keeps only exact fn-name matches — the single-benchmark seam editors use, and the
    // impact-filtered `--watch` consumer (server-hmr W3).
    let selected: Vec<&TierFn> = if names.is_empty() {
        activated.benches.iter().collect()
    } else {
        activated
            .benches
            .iter()
            .filter(|b| names.iter().any(|n| n == &b.name))
            .collect()
    };
    if selected.is_empty() {
        if names.is_empty() {
            println!("no benchmarks found");
        } else {
            println!("no benchmarks matching --name");
        }
        return ExitCode::SUCCESS;
    }

    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    let base = match baseline {
        Some(name) => match load_bench_baseline(file, name) {
            Ok(map) => Some(map),
            Err(err) => {
                eprintln!("noeta: {err}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    let total = selected.len();
    if !json {
        println!("running {total} benchmark{}", plural(total));
    }

    let mut outcomes: Vec<BenchOutcome> = Vec::with_capacity(total);
    for bench in &selected {
        let n = iterations_override
            .or_else(|| iterations_arg(bench))
            .map(|n| n.max(1))
            .unwrap_or_else(|| calibrate_iterations(&setup, &run.editions, bench));
        let mut outcome = match (
            measure_iterations(&setup, &run.editions, bench, n, 3),
            measure_iterations(&setup, &run.editions, bench, n.saturating_mul(2), 3),
        ) {
            (Ok(t1), Ok(t2)) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                per_iter_ns: Some(
                    ((t2.as_nanos() as f64 - t1.as_nanos() as f64) / n as f64).max(0.0),
                ),
                message: None,
                baseline_delta_pct: None,
            },
            (Err(msg), _) | (_, Err(msg)) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                per_iter_ns: None,
                message: Some(msg),
                baseline_delta_pct: None,
            },
        };
        if let (Some(base), Some(cur)) = (&base, outcome.per_iter_ns)
            && let Some(prev) = base.get(&outcome.name).copied()
            && prev > 0.0
        {
            outcome.baseline_delta_pct = Some((cur - prev) / prev * 100.0);
        }
        if !json {
            print_bench_outcome(&outcome, baseline.as_deref());
        }
        outcomes.push(outcome);
    }

    if let Some(name) = save_baseline
        && let Err(err) = save_bench_baseline(file, name, &outcomes)
    {
        eprintln!("noeta: cannot save baseline `{name}`: {err}");
        return ExitCode::from(2);
    }

    let failed = outcomes.iter().filter(|o| o.per_iter_ns.is_none()).count();
    // The CI gate: any bench past the allowed regression fails the run (their names on stderr —
    // the JSON stays a pure result object either way).
    let regressed: Vec<&BenchOutcome> = match max_regress {
        Some(limit) => outcomes
            .iter()
            .filter(|o| o.baseline_delta_pct.is_some_and(|pct| pct > limit))
            .collect(),
        None => Vec::new(),
    };
    if json {
        let out = serde_json::json!({
            "benches": outcomes.iter().map(|o| serde_json::json!({
                "name": o.name,
                "iterations": o.iterations,
                "perIterNs": o.per_iter_ns,
                "message": o.message,
                "baselineDeltaPct": o.baseline_delta_pct,
            })).collect::<Vec<_>>(),
            "ran": total - failed,
            "failed": failed,
            "regressed": regressed.len(),
            "total": total,
        });
        println!("{out}");
    } else {
        println!();
        println!("{} ran, {failed} failed, {total} total", total - failed,);
    }
    for o in &regressed {
        eprintln!(
            "noeta: `{}` regressed {:+.1}% (limit {:+.1}%)",
            o.name,
            o.baseline_delta_pct.unwrap_or_default(),
            max_regress.unwrap_or_default(),
        );
    }
    let _ = io::stdout().flush();
    if failed == 0 && regressed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// One line of the human bench report: per-iteration time, iteration count, and — under
/// `--baseline` — the percent delta against the named baseline.
pub(crate) fn print_bench_outcome(outcome: &BenchOutcome, baseline: Option<&str>) {
    match (outcome.per_iter_ns, &outcome.message) {
        (Some(per_ns), _) => {
            let delta = match (outcome.baseline_delta_pct, baseline) {
                (Some(pct), Some(name)) => format!("  ({pct:+.1}% vs {name})"),
                _ => String::new(),
            };
            println!(
                "  {:<28} {:>11}/iter  ({} iterations){delta}",
                outcome.name,
                fmt_per_iter(per_ns),
                outcome.iterations,
            );
        }
        (None, msg) => {
            println!(
                "  {:<28} FAILED: {}",
                outcome.name,
                msg.as_deref().unwrap_or("unknown")
            );
        }
    }
}

/// Size the iteration count so **one measurement run** takes at least
/// [`BENCH_TARGET_POINT_NS`] (the criterion/go-bench growth loop): run once at a small count and
/// scale up toward the target until a run crosses it (or the count hits the ceiling). Sizing on
/// the *total* run time is deliberately simple — startup rides along, but once a run is ≥ 50ms
/// the two-point subtraction in the real measurement operates far above scheduler/startup noise,
/// which is the property that makes the reported per-iteration number stable. A failing probe
/// falls back to [`DEFAULT_BENCH_ITERATIONS`] (the bench fails identically in the real
/// measurement, where it is reported).
pub(crate) fn calibrate_iterations(
    setup: &[Stmt],
    editions: &noeta_lexer::EditionMap,
    bench: &TierFn,
) -> u64 {
    let mut n: u64 = 64;
    loop {
        let Ok(t) = measure_iterations(setup, editions, bench, n, 1) else {
            return DEFAULT_BENCH_ITERATIONS;
        };
        let elapsed = t.as_nanos().max(1) as f64;
        if elapsed >= BENCH_TARGET_POINT_NS || n >= BENCH_MAX_ITERATIONS {
            return n;
        }
        // Jump toward the target in one step, but at most 16× (a first run dominated by startup
        // under-reports per-iteration cost, so an unbounded jump could overshoot the target run
        // time badly); at least 2× so the loop always terminates.
        let factor = (BENCH_TARGET_POINT_NS / elapsed).ceil().clamp(2.0, 16.0) as u64;
        n = n.saturating_mul(factor).min(BENCH_MAX_ITERATIONS);
    }
}

/// The baseline file for `entry` + `name`: per-entry (hashed canonical path) under the noeta
/// cache directory — baselines are machine-local timings, not project artifacts.
pub(crate) fn bench_baseline_path(entry: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let canonical = entry
        .canonicalize()
        .map_err(|e| format!("cannot resolve `{}`: {e}", entry.display()))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    let dir = noeta_cache::Cache::locate()
        .ok_or_else(|| "no cache directory available for baselines".to_string())?
        .join("bench-baselines");
    Ok(dir.join(format!("{:016x}-{name}.json", hasher.finish())))
}

/// Persist a run's successful measurements as the named baseline (`name → per-iteration ns`).
pub(crate) fn save_bench_baseline(
    entry: &std::path::Path,
    name: &str,
    outcomes: &[BenchOutcome],
) -> Result<(), String> {
    let path = bench_baseline_path(entry, name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let map: serde_json::Map<String, serde_json::Value> = outcomes
        .iter()
        .filter_map(|o| {
            o.per_iter_ns
                .map(|ns| (o.name.clone(), serde_json::json!(ns)))
        })
        .collect();
    let doc = serde_json::json!({ "benches": map });
    std::fs::write(&path, doc.to_string()).map_err(|e| e.to_string())
}

/// Load the named baseline for `entry` (`name → per-iteration ns`).
pub(crate) fn load_bench_baseline(
    entry: &std::path::Path,
    name: &str,
) -> Result<std::collections::HashMap<String, f64>, String> {
    let path = bench_baseline_path(entry, name)?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no baseline `{name}` saved for this file (expected `{}`)",
            path.display()
        )
    })?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("corrupt baseline `{name}`: {e}"))?;
    Ok(doc["benches"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default())
}

/// The bench's `#[Bench(iterations: N)]` knob, if present and positive — the per-bench override of
/// the default iteration count. The attribute is either written on the fn directly or stamped from
/// the block's `@bench(…)` directive args (activation's desugar), and the construction gate has
/// already validated it, so this only reads: `iterations` is `Bench`'s sole field, bound
/// positionally (`#[Bench(1000)]` / `@bench(1000)`) or by name.
pub(crate) fn iterations_arg(bench: &TierFn) -> Option<u64> {
    let attr = bench
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TIER_ATTR_BENCH)?;
    let value = attr
        .args
        .iter()
        .find(|arg| matches!(&arg.name, Some(n) if n == "iterations") || arg.name.is_none())?;
    match value.value {
        AttrValue::Int(n) if n > 0 => Some(n as u64),
        _ => None,
    }
}

/// Measure executing `bench` `n` times: synthesize `setup + n×<call the bench fn>`, then run it in a
/// fresh real-host isolate, timing **only execution** (IR lowering is done untimed, before the
/// clock starts). A discarded warm-up run plus the minimum of three measured runs damps noise. A
/// nonzero exit / any diagnostic (a panic in the bench body) is a failure, surfaced as `Err`.
pub(crate) fn measure_iterations(
    setup: &[Stmt],
    editions: &noeta_lexer::EditionMap,
    bench: &TierFn,
    n: u64,
    runs: u32,
) -> Result<std::time::Duration, String> {
    // The measured program is `setup` + one counted loop over the bench call — a *loop*, not `n`
    // synthesized call statements, so the VM treats both measurement points identically (the same
    // loop body crosses the JIT threshold the same way at N and 2N; only the trip count differs,
    // and the warm-up cost cancels in the two-point subtraction exactly like startup does). The
    // range list materializes once, before the clock-relevant body, and scales linearly with `n`,
    // so its cost lands as a tiny constant inside the per-iteration figure rather than skewing
    // the subtraction.
    let mut stmts = setup.to_vec();
    let span = bench.span;
    stmts.push(Stmt::For {
        pattern: noeta_ast::ForPattern::Single {
            name: "__bench_iter".to_string(),
            name_span: span,
        },
        iterable: Expr::Range {
            start: Box::new(Expr::Int { value: 0, span }),
            end: Box::new(Expr::Int {
                value: n as i64,
                span,
            }),
            inclusive: false,
            span,
        },
        body: vec![call_stmt(&bench.name, Vec::new(), span)],
        span,
    });
    let program = Program {
        stmts,
        span: bench.span,
    };

    let checked = check_under(&program, editions);
    if !checked.diagnostics.is_empty() {
        return Err(checked.diagnostics[0].message.clone());
    }

    // Take the minimum of three runs: `min` is the standard robust estimator (the fastest run is
    // the one least perturbed by scheduler/GC/OS noise) and inherently discards the cold first run,
    // so no separate warm-up is needed.
    let mut best: Option<std::time::Duration> = None;
    for _ in 0..runs.max(1) {
        let (result, elapsed) = bench_execute(&program, &checked)?;
        if result.exit_code != 0 || !result.diagnostics.is_empty() {
            return Err(result
                .diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| format!("exited with code {}", result.exit_code)));
        }
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    Ok(best.expect("at least one measured run"))
}

/// Lower a program for the real host (untimed) and execute it, returning the result and the
/// **execution-only** wall-clock duration (lowering excluded). Mirrors [`execute_real_host`]'s
/// pipeline so a benchmark runs the same Core-IR path a normal `noeta run` does.
pub(crate) fn bench_execute(
    program: &Program,
    checked: &noeta_check::Checked,
) -> Result<(noeta_backend::RunResult, std::time::Duration), String> {
    let host =
        noeta_runtime::RealHost::new().map_err(|err| format!("cannot start the runtime: {err}"))?;
    // Compile to bytecode untimed (isolates I.4a — the real path is the VM), then time execution
    // alone, so the measurement excludes both lowering and bytecode generation.
    let module = compile_real(program, checked)?;
    let start = Instant::now();
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    Ok((result, start.elapsed()))
}

/// Format a per-iteration duration (in nanoseconds) with an adaptive unit, so a fast op reads in
/// `ns` and a slow one in `ms`/`s`.
pub(crate) fn fmt_per_iter(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}
