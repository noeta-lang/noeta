//! `noeta bench` — measure a program's `@bench` blocks with two-point calibrated runs,
//! and persist/diff machine-local baselines.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

use noeta_ast::{AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_runner::compile_real;
use noeta_vm::VmBackend;

use crate::cmd::test::call_root_stmt_awaited;
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
    /// The per-iteration cost. `None` when the bench failed (see `message`); `Some(0.0)` when it ran
    /// but the two-point subtraction did not resolve, which is **not** a measurement — see
    /// [`resolved`] and [`UNRESOLVED_NOTE`], and note that every consumer has to ask, because a zero
    /// here reads as "immeasurably fast" and used to be persisted as a baseline on that reading.
    per_iter_ns: Option<f64>,
    message: Option<String>,
    /// Percent change vs the `--baseline` entry, when one exists for this bench.
    baseline_delta_pct: Option<f64>,
    /// Why there is **no** `baseline_delta_pct` even though `--baseline` asked for one. A comparison
    /// the user requested and did not get is a thing to say out loud, not to omit: silently dropping
    /// the delta is how a useless baseline stayed invisible.
    baseline_note: Option<String>,
}

/// Everything a bench run carries besides the path it runs over — threaded as one value so the
/// file and directory paths take the same options without a wall of parameters.
pub(crate) struct BenchOptions<'a> {
    iterations_override: Option<u64>,
    names: &'a [String],
    json: bool,
    save_baseline: &'a Option<String>,
    baseline: &'a Option<String>,
    max_regress: Option<f64>,
    target: &'a Option<String>,
}

/// What one file contributed to a bench run.
enum FileBenches {
    /// The tier prologue short-circuited (delegation, a `--target` that does not make `bench`
    /// live, or a rendered diagnostic). Carries its exit code.
    Ran(u8),
    /// The file ran but selected no benchmarks. `any_declared` separates "declares none" from
    /// "declares some, and `--name` kept none".
    None { any_declared: bool },
    /// The file's selected benchmarks were measured.
    Collected(Vec<BenchOutcome>),
}

/// `noeta bench [PATH]` — discover `@bench` blocks (object-model slice 6) and measure each. Unlike
/// `noeta test`, benchmarks run **sequentially** (concurrency would corrupt timings). Each bench's
/// per-iteration cost is estimated by a **two-point** measurement: the fn is invoked N and 2N times
/// in fresh isolates and the per-iteration time is `(t(2N) − t(N)) / N`, which cancels the fixed
/// per-run overhead (runtime startup, global/setup evaluation, IR lowering — all identical between
/// the two runs, unless the machine is contended enough to break that premise — see
/// [`measure_per_iter`], which retries a subtraction that does not resolve). N comes from
/// `--iterations`, else the per-bench `@bench(iterations: N)`
/// directive, else **calibration** (a two-run probe sizes N so one point takes
/// [`BENCH_TARGET_POINT_NS`]). `--name` filters (exact fn-name match, repeatable); `--json`
/// reports one machine-readable object; `--save-baseline`/`--baseline` persist and diff runs.
///
/// `PATH` (default `.`) is a file or a **directory**, mirroring `noeta check`/`noeta test`. A
/// directory measures every `.noe` beneath it as its own entry — the only way a multi-module
/// project's benchmarks all run, since linking merges a sibling's declarations without its
/// `@bench` blocks. Baselines stay **per entry file** (they are keyed by it), which is what makes
/// a directory run's `--baseline`/`--save-baseline` compare like with like.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_bench(
    path: &std::path::Path,
    iterations_override: Option<u64>,
    names: &[String],
    json: bool,
    save_baseline: &Option<String>,
    baseline: &Option<String>,
    max_regress: Option<f64>,
    target: &Option<String>,
) -> u8 {
    let opts = BenchOptions {
        iterations_override,
        names,
        json,
        save_baseline,
        baseline,
        max_regress,
        target,
    };
    if path.is_dir() {
        return bench_directory(path, &opts);
    }
    match run_file_benches(path, &opts, None) {
        FileBenches::Ran(code) => code,
        FileBenches::None { any_declared } => {
            println!("{}", empty_bench_message(any_declared, names));
            0
        }
        FileBenches::Collected(outcomes) => report_benches(&outcomes, &opts, 0),
    }
}

/// The message for a run that selected no benchmarks.
fn empty_bench_message(any_declared: bool, names: &[String]) -> &'static str {
    if any_declared && !names.is_empty() {
        "no benchmarks matching --name"
    } else {
        "no benchmarks found"
    }
}

/// Measure every `.noe` file under `dir` as its own entry, into one report and one exit code.
///
/// Outcomes are labelled with the file they came from, so a bench name shared by two modules stays
/// distinguishable. Labelling happens *after* each file's baseline load/save, which key on the
/// bare fn name — so a directory run diffs against exactly the baselines a per-file run wrote.
fn bench_directory(dir: &std::path::Path, opts: &BenchOptions) -> u8 {
    if crate::compose::maybe_delegate(dir).is_err() {
        // Composition needed but failed (a fixed exit-1 delegation); the tier subsystem is `u8`.
        return 1;
    }
    let mut outcomes: Vec<BenchOutcome> = Vec::new();
    let mut broken = 0usize;
    let mut any_declared = false;
    for file in &crate::cmd::check::noe_files(dir) {
        let label = file
            .strip_prefix(dir)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned();
        match run_file_benches(file, opts, Some(&label)) {
            FileBenches::Ran(code) => {
                if code != 0 {
                    broken += 1;
                }
            }
            FileBenches::None { any_declared: d } => any_declared |= d,
            FileBenches::Collected(o) => outcomes.extend(o),
        }
    }
    if outcomes.is_empty() && broken == 0 {
        println!("{}", empty_bench_message(any_declared, opts.names));
        return 0;
    }
    report_benches(&outcomes, opts, broken)
}

/// Measure one file's `@bench` blocks. The single-file body of [`cmd_bench`], factored out so the
/// directory walk can reuse it; `label`, when given, prefixes each reported name with its file.
fn run_file_benches(
    file: &std::path::Path,
    opts: &BenchOptions,
    label: Option<&str>,
) -> FileBenches {
    // The shared tier prologue: compose delegation, the `--target` gate, the dep-aware load,
    // provider dispatch (a `bench = "<pkg>"` target hands the tier to that package's runner),
    // activation diagnostics, and the whole-program type check.
    let run = match tier_prologue(file, "bench", opts.target) {
        Prologue::Ran(code) => return FileBenches::Ran(code),
        Prologue::Ready(run) => *run,
    };
    let activated = &run.activated;

    // `--name` keeps only exact fn-name matches — the single-benchmark seam editors use, and the
    // impact-filtered `--watch` consumer (server-hmr W3).
    let selected: Vec<&TierFn> = if opts.names.is_empty() {
        activated.benches.iter().collect()
    } else {
        activated
            .benches
            .iter()
            .filter(|b| opts.names.iter().any(|n| n == &b.name))
            .collect()
    };
    if selected.is_empty() {
        return FileBenches::None {
            any_declared: !activated.benches.is_empty(),
        };
    }

    // The same shared-setup policy `noeta test` uses, from the same place: a top-level effect runs
    // unless it does not return. A bench that measures against a fixture the file sets up gets that
    // fixture; a file whose top level calls `server.serve(…)` still never enters the accept loop.
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| noeta_check::is_tier_setup(s, &run.diverging))
        .cloned()
        .collect();

    let base = match opts.baseline {
        Some(name) => match load_bench_baseline(file, name) {
            Ok(map) => Some(map),
            Err(err) => {
                eprintln!("noeta: {err}");
                return FileBenches::Ran(2);
            }
        },
        None => None,
    };

    let total = selected.len();
    if !opts.json {
        let in_file = label.map(|l| format!(" in {l}")).unwrap_or_default();
        println!("running {total} benchmark{}{in_file}", plural(total));
    }

    let mut outcomes: Vec<BenchOutcome> = Vec::with_capacity(total);
    for bench in &selected {
        let n = opts
            .iterations_override
            .or_else(|| iterations_arg(bench))
            .map(|n| n.max(1))
            .unwrap_or_else(|| calibrate_iterations(&setup, &run.opts, bench));
        let mut outcome = match measure_per_iter(&setup, &run.opts, bench, n) {
            Ok(per_iter_ns) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                // Zero here means **unresolved** rather than free — every attempt at the subtraction
                // came out at or below zero. Everything that could mistake it for a measurement tests
                // `resolved()`: the report line, the baseline comparison, the baseline *save*, and
                // the `--max-regress` gate.
                per_iter_ns: Some(per_iter_ns),
                message: None,
                baseline_delta_pct: None,
                baseline_note: None,
            },
            Err(msg) => BenchOutcome {
                name: bench.name.clone(),
                iterations: n,
                per_iter_ns: None,
                message: Some(msg),
                baseline_delta_pct: None,
                baseline_note: None,
            },
        };
        if let Some(base) = &base {
            compare_to_baseline(&mut outcome, base);
        }
        if !opts.json {
            print_bench_outcome(&outcome, opts.baseline.as_deref(), label);
        }
        outcomes.push(outcome);
    }

    // Save BEFORE labelling: a baseline is keyed by the bare fn name, per entry file, so the file
    // a directory run wrote is byte-identical to the one `noeta bench <that file>` writes.
    if let Some(name) = opts.save_baseline
        && let Err(err) = save_bench_baseline(file, name, &outcomes)
    {
        eprintln!("noeta: cannot save baseline `{name}`: {err}");
        return FileBenches::Ran(2);
    }
    if let Some(l) = label {
        for outcome in &mut outcomes {
            outcome.name = format!("{l}::{}", outcome.name);
        }
    }
    FileBenches::Collected(outcomes)
}

/// How many independent two-point attempts a measurement gets before its non-resolution is reported.
///
/// A subtraction that lands at or below zero is not a fact about the program; it is a **spoiled
/// sample**. The two-point method's premise is that the fixed per-run overhead is the same at N and
/// 2N, and CPU contention breaks that premise directly: the N point can draw a luckier set of time
/// slices than the 2N point and come out relatively slower, inverting a difference of any size.
///
/// Measured on a 20-core box under 3× oversubscription (24 spinning burners on top of the ambient
/// load), 40 samples per body, one attempt each:
///
/// | body per point | unresolved | wall per measurement |
/// |---------------:|-----------:|---------------------:|
/// |         ~50 ms |       2/40 |                4.2 s |
/// |        ~100 ms |       3/40 |                8.4 s |
///
/// **The rate does not fall as the body grows** — doubling the work doubled the wall time and moved
/// nothing — which is what identifies the noise as multiplicative rather than additive, and is why
/// "give the body more work" was never going to fix it. Repetition is the only lever that bites: three
/// independent attempts take a few percent down to a few parts in ten thousand.
///
/// The cost is paid only where the first attempt fails, which for an honestly-too-fast body (a smoke
/// bench at `iterations: 5`) means three attempts at a body that is nearly free anyway.
const BENCH_MEASUREMENT_ATTEMPTS: u32 = 3;

/// Estimate `bench`'s per-iteration cost: `(t(2N) − t(N)) / N`, retried up to
/// [`BENCH_MEASUREMENT_ATTEMPTS`] times while the subtraction does not resolve.
///
/// Returns `0.0` — the absence of a measurement, see [`UNRESOLVED_NOTE`] — when every
/// attempt lands at or below zero. A bench whose *body* fails is an `Err` on the first attempt and is
/// never retried: that is a fact about the program, not a spoiled sample.
fn measure_per_iter(
    setup: &[Stmt],
    opts: &noeta_check::CheckOptions,
    bench: &TierFn,
    n: u64,
) -> Result<f64, String> {
    for _ in 0..BENCH_MEASUREMENT_ATTEMPTS {
        let t1 = measure_iterations(setup, opts, bench, n, 3)?;
        let t2 = measure_iterations(setup, opts, bench, n.saturating_mul(2), 3)?;
        let per_iter_ns = (t2.as_nanos() as f64 - t1.as_nanos() as f64) / n as f64;
        if resolved(per_iter_ns) {
            return Ok(per_iter_ns);
        }
    }
    Ok(0.0)
}

/// What a per-iteration figure of `0.0` means, and what to do about it.
///
/// The two-point subtraction is only meaningful when the 2N run actually took longer than the N run.
/// It often does not, for either of two unrelated reasons: a body far cheaper than the fixed per-run
/// overhead is measured almost entirely in noise, and a machine under contention can hand the two
/// points different amounts of CPU. Both land the difference at or below zero, which
/// [`measure_per_iter`] reports as exactly `0.0` once every attempt has come out that way. That is not
/// a measurement of zero, it is the absence of a measurement — and it is *routine*, not exceptional
/// (a smoke bench at `iterations: 5` lands here every run), which is why it is a note on the report
/// line rather than a bench failure.
///
/// The advice names all three fixes, because by the time this note is printed
/// [`BENCH_MEASUREMENT_ATTEMPTS`] independent attempts have already failed — at which point "your
/// benchmark is too cheap" is a claim worth making, and so is "your machine is too busy".
const UNRESOLVED_NOTE: &str = "no per-iteration cost resolved above the timer noise — raise `--iterations`, give the body \
     more work, or measure on a less contended machine";

/// Whether a per-iteration figure is a real measurement. Zero is how a two-point measurement says
/// "the subtraction did not resolve" (see [`UNRESOLVED_NOTE`]); a genuine measurement is strictly positive,
/// since the smallest nonzero nanosecond difference divided by `n` still is.
fn resolved(per_iter_ns: f64) -> bool {
    per_iter_ns > 0.0
}

/// Fill in `outcome`'s comparison against a loaded `--baseline`: the percent delta when **both** sides
/// have a real measurement, and otherwise a note saying why there is none.
///
/// The `else` branches are the point. `--baseline <name>` is a request for a comparison, so every way
/// of not producing one has to be visible in the report. Three ways it silently was not: a missing
/// entry (a benchmark added since the baseline was saved), a stored entry of `0.0` (persisted by a
/// version that saved the clamp's output — the `prev > 0.0` test dropped the delta without a word),
/// and — worse than dropping it — an *unresolved current* measurement compared against a real
/// baseline, which produced a confident `-100.0%`.
fn compare_to_baseline(outcome: &mut BenchOutcome, base: &std::collections::HashMap<String, f64>) {
    let Some(cur) = outcome.per_iter_ns else {
        // This run has no measurement at all; its own failure message is the report.
        return;
    };
    if !resolved(cur) {
        outcome.baseline_note = Some(format!("this run measured nothing — {UNRESOLVED_NOTE}"));
        return;
    }
    match base.get(&outcome.name).copied() {
        Some(prev) if resolved(prev) => {
            outcome.baseline_delta_pct = Some((cur - prev) / prev * 100.0);
        }
        Some(prev) => {
            outcome.baseline_note = Some(format!(
                "the stored baseline is {prev} ns/iter, which nothing can be compared against; \
                 re-save it"
            ));
        }
        None => {
            outcome.baseline_note =
                Some("this baseline has no entry for this benchmark".to_string());
        }
    }
}

/// The exit code for a `--max-regress` run that could not be judged: the gate did not *fail*, it did
/// not *run*. `1` already means "measured, and a benchmark regressed", and `2` is already this
/// command's "could not do what you asked" (an unknown `--baseline`, an unwritable baseline file), so
/// the two answers stay distinguishable by exit code alone — which is the whole point of a gate.
const BENCH_GATE_INCONCLUSIVE: u8 = 2;

/// Print the summary (human or `--json`) and decide the exit code. `broken` counts files whose
/// prologue failed — they rendered their own diagnostics and fail the run.
fn report_benches(outcomes: &[BenchOutcome], opts: &BenchOptions, broken: usize) -> u8 {
    let total = outcomes.len();
    let failed = outcomes.iter().filter(|o| o.per_iter_ns.is_none()).count();
    // The CI gate: any bench past the allowed regression fails the run (their names on stderr —
    // the JSON stays a pure result object either way).
    let regressed: Vec<&BenchOutcome> = match opts.max_regress {
        Some(limit) => outcomes
            .iter()
            .filter(|o| o.baseline_delta_pct.is_some_and(|pct| pct > limit))
            .collect(),
        None => Vec::new(),
    };
    // …and any bench the gate was asked about but could not judge. A gate exists so that nobody has
    // to read stdout, so "no delta came out of the comparison" must not be reported as `0` — that is
    // indistinguishable from "measured, and fine", and it is exactly what happened here: a body too
    // cheap for the two-point subtraction to resolve produced no measurement, no delta, no regression,
    // and a green gate. Ungated benches are named on stderr and the run exits
    // [`BENCH_GATE_INCONCLUSIVE`].
    //
    // Note this is only the *gate's* rule. A plain `--baseline` run is a report, and a report that
    // prints why a comparison did not happen (the `no comparison vs <name>: …` note) is not silent —
    // it stays exit `0`. The exit code changes only where the exit code is the product.
    let ungated: Vec<&BenchOutcome> = match opts.max_regress {
        // A bench that failed outright is already counted in `failed`; its own message is the report.
        Some(_) => outcomes
            .iter()
            .filter(|o| o.per_iter_ns.is_some() && o.baseline_delta_pct.is_none())
            .collect(),
        None => Vec::new(),
    };
    if opts.json {
        let out = serde_json::json!({
            "benches": outcomes.iter().map(|o| serde_json::json!({
                "name": o.name,
                "iterations": o.iterations,
                "perIterNs": o.per_iter_ns,
                // Whether `perIterNs` is a measurement at all. `0` is how the runner says the
                // two-point subtraction did not resolve, and a consumer charting the series has to be
                // able to drop that point rather than plot a benchmark that got infinitely fast.
                "unresolved": o.per_iter_ns.is_some_and(|ns| !resolved(ns)),
                "message": o.message,
                "baselineDeltaPct": o.baseline_delta_pct,
                // Why `baselineDeltaPct` is null under `--baseline`, when it is. A consumer that
                // charts deltas needs to be able to tell "unchanged" from "never compared".
                "baselineNote": o.baseline_note,
            })).collect::<Vec<_>>(),
            "ran": total - failed,
            "failed": failed,
            "regressed": regressed.len(),
            // How many benchmarks `--max-regress` could not judge. A gate consumer reading the JSON
            // rather than the exit code needs the same fact the exit code carries.
            "ungated": ungated.len(),
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
            opts.max_regress.unwrap_or_default(),
        );
    }
    for o in &ungated {
        eprintln!(
            "noeta: `{}` was not compared, so `--max-regress` could not judge it: {}",
            o.name,
            o.baseline_note
                .as_deref()
                .unwrap_or("no delta against the baseline"),
        );
    }
    if !ungated.is_empty() {
        eprintln!(
            "noeta: the regression gate is inconclusive: {} of {total} benchmark{} could not be \
             compared, so a pass here would prove nothing (exit {BENCH_GATE_INCONCLUSIVE})",
            ungated.len(),
            plural(total),
        );
    }
    if broken > 0 {
        eprintln!(
            "noeta: {broken} file{} failed to check; {} benchmarks did not run",
            plural(broken),
            if broken == 1 { "its" } else { "their" }
        );
    }
    let _ = io::stdout().flush();
    // A found regression outranks an inconclusive one: `1` is the more actionable answer, and a run
    // that saw a real regression *did* gate.
    if failed > 0 || !regressed.is_empty() || broken > 0 {
        1
    } else if ungated.is_empty() {
        0
    } else {
        BENCH_GATE_INCONCLUSIVE
    }
}

/// One line of the human bench report: per-iteration time, iteration count, and — under
/// `--baseline` — the percent delta against the named baseline.
pub(crate) fn print_bench_outcome(
    outcome: &BenchOutcome,
    baseline: Option<&str>,
    label: Option<&str>,
) {
    // The stored name stays bare until the baseline has been saved (it is the baseline's key), so
    // a directory run's per-line label is applied here, at display time, to match the summary and
    // the `--json` names.
    let name = match label {
        Some(l) => format!("{l}::{}", outcome.name),
        None => outcome.name.clone(),
    };
    match (outcome.per_iter_ns, &outcome.message) {
        (Some(per_ns), _) => println!(
            "  {:<28} {:>11}/iter  ({} iterations){}",
            name,
            fmt_per_iter(per_ns),
            outcome.iterations,
            report_suffix(outcome, per_ns, baseline),
        ),
        (None, msg) => {
            println!(
                "  {:<28} FAILED: {}",
                name,
                msg.as_deref().unwrap_or("unknown")
            );
        }
    }
}

/// What follows the iteration count on a report line: the `--baseline` delta, the reason there is no
/// delta, and — when the run resolved nothing — what that zero means.
fn report_suffix(outcome: &BenchOutcome, per_ns: f64, baseline: Option<&str>) -> String {
    let delta = match (outcome.baseline_delta_pct, baseline) {
        (Some(pct), Some(name)) => format!("  ({pct:+.1}% vs {name})"),
        // No delta, but one was asked for: say which comparison did not happen and why, rather than
        // printing a line that looks like an ordinary uncompared run.
        (None, Some(name)) => match &outcome.baseline_note {
            Some(note) => format!("  (no comparison vs {name}: {note})"),
            None => String::new(),
        },
        _ => String::new(),
    };
    // `0 ns/iter` on its own reads as "immeasurably fast", which is the opposite of what it means.
    // Say what it means. The line keeps its shape — the number, the unit and the iteration count —
    // so this is a note, not a new report format.
    //
    // Once, though: under `--baseline` the "this run measured nothing — …" note already carries this
    // text, and appending it again printed the same sentence twice on one line.
    if resolved(per_ns) || delta.contains(UNRESOLVED_NOTE) {
        delta
    } else {
        format!("{delta}  ({UNRESOLVED_NOTE})")
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
    opts: &noeta_check::CheckOptions,
    bench: &TierFn,
) -> u64 {
    let mut n: u64 = 64;
    loop {
        let Ok(t) = measure_iterations(setup, opts, bench, n, 1) else {
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
///
/// **Refuses to write a baseline nothing can be compared against**, and writes nothing at all when it
/// refuses — this is the one place an unresolved measurement is fatal rather than a note, because it is
/// the only one that outlives the run.
///
/// Two ways that used to happen quietly. A per-iteration cost of `0.0` — which a two-point
/// subtraction that does not resolve produces (see [`UNRESOLVED_NOTE`]) — was stored without
/// comment, and the next `--baseline <name>` then skipped the delta because `prev > 0.0` failed and
/// said nothing about why: a plain report, no error, no warning, no comparison. And a baseline with no
/// entries at all (every bench failed) is the same silence by a different route. Both surface days
/// later as "the run I asked to compare printed no comparison", which is exactly the report that took
/// two CI reds and eleven unreproducible runs to pin down.
pub(crate) fn save_bench_baseline(
    entry: &std::path::Path,
    name: &str,
    outcomes: &[BenchOutcome],
) -> Result<(), String> {
    let mut map = serde_json::Map::new();
    for outcome in outcomes {
        // A bench that failed outright is left out, as it always was: the run reports it and exits
        // nonzero, and its absence from the baseline is then explained by the compare-time note.
        let Some(ns) = outcome.per_iter_ns else {
            continue;
        };
        if !resolved(ns) {
            return Err(format!(
                "`{}` measured {ns} ns/iter — {UNRESOLVED_NOTE}. Saving it would make every later \
                 comparison against this baseline vacuous",
                outcome.name
            ));
        }
        map.insert(outcome.name.clone(), serde_json::json!(ns));
    }
    if map.is_empty() {
        return Err(format!(
            "no benchmark produced a usable measurement ({} ran)",
            outcomes.len()
        ));
    }
    let path = bench_baseline_path(entry, name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
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
    opts: &noeta_check::CheckOptions,
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
        body: vec![call_root_stmt_awaited(
            &bench.name,
            Vec::new(),
            span,
            bench.is_async,
        )],
        span,
    });
    let program = Program {
        stmts,
        span: bench.span,
    };

    let checked = check_under(&program, opts);
    // Only an error aborts the measurement. Warnings stay silent here for the same reason they do in
    // the `@test` runner: this synthesized per-case program re-checks source the prologue already
    // reported on, so repeating it would print each warning once per benchmark.
    if let Some(error) = checked.diagnostics.iter().find(|d| d.is_error()) {
        return Err(error.message.clone());
    }

    // Take the minimum of three runs: `min` is the standard robust estimator (the fastest run is
    // the one least perturbed by scheduler/GC/OS noise) and inherently discards the cold first run,
    // so no separate warm-up is needed.
    let mut best: Option<std::time::Duration> = None;
    for _ in 0..runs.max(1) {
        let (result, elapsed) = bench_execute(&program, &checked)?;
        // An abort invalidates the measurement; an advisory diagnostic does not.
        if result.exit_code != 0 || noeta_diagnostics::has_errors(&result.diagnostics) {
            return Err(result
                .diagnostics
                .iter()
                .find(|d| d.is_error())
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
    let host = noeta_host_real::RealHost::new()
        .map_err(|err| format!("cannot start the runtime: {err}"))?;
    // Compile to bytecode untimed (isolates I.4a — the real path is the VM), then time execution
    // alone, so the measurement excludes both lowering and bytecode generation.
    let module = compile_real(program, checked).map_err(|u| u.to_string())?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A `--baseline main [--max-regress <limit>]` run's options, borrowing the caller's owners.
    fn gate<'a>(
        baseline: &'a Option<String>,
        none: &'a Option<String>,
        limit: Option<f64>,
    ) -> BenchOptions<'a> {
        BenchOptions {
            iterations_override: None,
            names: &[],
            json: false,
            save_baseline: none,
            baseline,
            max_regress: limit,
            target: none,
        }
    }

    /// An outcome with a given measurement, as the two-point path would produce it.
    fn measured(name: &str, per_iter_ns: Option<f64>) -> BenchOutcome {
        BenchOutcome {
            name: name.to_string(),
            iterations: 100,
            per_iter_ns,
            message: None,
            baseline_delta_pct: None,
            baseline_note: None,
        }
    }

    #[test]
    fn zero_is_the_absence_of_a_measurement_not_a_fast_one() {
        // The whole bug turns on this distinction. `0.0` is what a two-point measurement reports when
        // the N run came out unluckier than the 2N run, which happens routinely for a body far cheaper
        // than the fixed per-run overhead — so it cannot be treated as a bench failure, and it must
        // not be treated as a number either.
        assert!(resolved(1.0));
        assert!(resolved(f64::MIN_POSITIVE));
        assert!(!resolved(0.0));
        assert!(!resolved(-1.0));
    }

    #[test]
    fn saving_a_baseline_refuses_a_measurement_of_nothing() {
        let entry = std::path::Path::new(file!());
        // A zero is refused by name, so the message says which bench and what to do about it.
        let err = save_bench_baseline(entry, "unit-test", &[measured("b", Some(0.0))])
            .expect_err("a zero baseline must be refused");
        assert!(err.contains("`b`"), "{err}");
        assert!(err.contains("vacuous"), "{err}");
        assert!(err.contains("--iterations"), "{err}");
        // So is a negative, which no honest measurement can be.
        assert!(save_bench_baseline(entry, "unit-test", &[measured("b", Some(-1.0))]).is_err());
        // And so is a baseline with no entries at all: every bench failed, so there is nothing to
        // compare against later — the same silence by a different route.
        let err = save_bench_baseline(entry, "unit-test", &[measured("b", None)])
            .expect_err("an empty baseline must be refused");
        assert!(
            err.contains("no benchmark produced a usable measurement"),
            "{err}"
        );
    }

    #[test]
    fn every_comparison_that_did_not_happen_says_why() {
        let mut base = HashMap::new();
        base.insert("good".to_string(), 100.0);
        base.insert("zero".to_string(), 0.0);

        let mut good = measured("good", Some(150.0));
        compare_to_baseline(&mut good, &base);
        assert_eq!(good.baseline_delta_pct, Some(50.0));
        assert!(good.baseline_note.is_none());

        // A stored entry of 0 — what an older `--save-baseline` could persist. The delta is undefined,
        // and *saying so* is the fix: it used to be dropped in silence.
        let mut zero = measured("zero", Some(150.0));
        compare_to_baseline(&mut zero, &base);
        assert!(zero.baseline_delta_pct.is_none());
        assert!(
            zero.baseline_note
                .as_deref()
                .is_some_and(|n| n.contains("re-save")),
            "{:?}",
            zero.baseline_note
        );

        // A benchmark added since the baseline was written.
        let mut fresh = measured("fresh", Some(150.0));
        compare_to_baseline(&mut fresh, &base);
        assert!(fresh.baseline_delta_pct.is_none());
        assert!(
            fresh
                .baseline_note
                .as_deref()
                .is_some_and(|n| n.contains("no entry")),
            "{:?}",
            fresh.baseline_note
        );

        // An unresolved *current* measurement against a real baseline. This one was worse than a
        // dropped delta: `(0 - 100) / 100` is a confident -100.0%, a benchmark reported as having got
        // infinitely faster.
        let mut unresolved = measured("good", Some(0.0));
        compare_to_baseline(&mut unresolved, &base);
        assert!(
            unresolved.baseline_delta_pct.is_none(),
            "an unresolved run must not report a delta: {:?}",
            unresolved.baseline_delta_pct
        );
        assert!(
            unresolved
                .baseline_note
                .as_deref()
                .is_some_and(|n| n.contains("measured nothing")),
            "{:?}",
            unresolved.baseline_note
        );

        // A bench with no measurement of its own has its own failure message; no baseline note is
        // added on top of it.
        let mut failed = measured("good", None);
        compare_to_baseline(&mut failed, &base);
        assert!(failed.baseline_note.is_none());
    }

    /// An outcome that measured and compared cleanly, at `pct` against the baseline.
    fn compared(name: &str, pct: f64) -> BenchOutcome {
        let mut o = measured(name, Some(120.0));
        o.baseline_delta_pct = Some(pct);
        o
    }

    /// An outcome that measured but produced no delta — the shape every "cannot compare" path lands
    /// in, whichever of them produced it.
    fn uncompared(name: &str, note: &str) -> BenchOutcome {
        let mut o = measured(name, Some(0.0));
        o.baseline_note = Some(note.to_string());
        o
    }

    #[test]
    fn a_gate_that_could_not_compare_does_not_pass() {
        // The defect this constant exists for: `--max-regress` returned `0` for a run that measured
        // nothing. No measurement ⇒ no delta ⇒ nothing above the limit ⇒ a green gate, byte-identical
        // by exit code to "measured, and fine". A gate is consulted precisely so nobody reads stdout,
        // so the inconclusive case has to have its own code.
        let baseline = Some("main".to_string());
        let none = None;
        let opts = gate(&baseline, &none, Some(10.0));

        // Measured and inside the limit: the gate passes, as it always did.
        assert_eq!(report_benches(&[compared("b", 3.0)], &opts, 0), 0);
        // Measured and over it: still `1`, and still the more actionable answer when both are true.
        assert_eq!(report_benches(&[compared("b", 40.0)], &opts, 0), 1);
        assert_eq!(
            report_benches(
                &[compared("b", 40.0), uncompared("c", "measured nothing")],
                &opts,
                0
            ),
            1
        );

        // Measured nothing, so compared nothing. This used to be `0`.
        assert_eq!(
            report_benches(&[uncompared("b", "this run measured nothing")], &opts, 0),
            BENCH_GATE_INCONCLUSIVE
        );
        // The neighbouring ways of not comparing are the same verdict: a bench the baseline never
        // knew about, and a stored baseline of `0.0` that nothing can be compared against.
        assert_eq!(
            report_benches(&[uncompared("b", "no entry for this benchmark")], &opts, 0),
            BENCH_GATE_INCONCLUSIVE
        );
        // A bench that failed outright is a failure, not an inconclusive gate — it already has a code.
        assert_eq!(report_benches(&[measured("b", None)], &opts, 0), 1);
    }

    #[test]
    fn the_unresolved_note_is_printed_once_per_line() {
        // Under `--baseline` the "no comparison" note already spells out what the zero means, and the
        // standalone note was appended on top of it — the same sentence twice on one report line.
        let unresolved = uncompared(
            "b",
            &format!("this run measured nothing — {UNRESOLVED_NOTE}"),
        );
        let line = report_suffix(&unresolved, 0.0, Some("main"));
        assert_eq!(
            line.matches(UNRESOLVED_NOTE).count(),
            1,
            "the note is printed twice: {line}"
        );
        assert!(line.contains("no comparison vs main"), "{line}");

        // Without `--baseline` there is no note to carry it, so the standalone one is what says it.
        let bare = measured("b", Some(0.0));
        let line = report_suffix(&bare, 0.0, None);
        assert_eq!(line.matches(UNRESOLVED_NOTE).count(), 1, "{line}");

        // A resolved run under `--baseline` is just the delta.
        let good = compared("b", -5.2);
        assert_eq!(
            report_suffix(&good, 120.0, Some("main")),
            "  (-5.2% vs main)"
        );
    }

    #[test]
    fn a_plain_baseline_run_is_a_report_not_a_gate() {
        // Without `--max-regress` nothing is being gated, so an uncomparable bench stays exit `0`:
        // the report already prints *why* it did not compare, and a report that says so is not
        // silent. Only the gate's verdict is carried by the exit code.
        let baseline = Some("main".to_string());
        let none = None;
        let opts = gate(&baseline, &none, None);
        assert_eq!(
            report_benches(&[uncompared("b", "this run measured nothing")], &opts, 0),
            0
        );
        assert_eq!(report_benches(&[compared("b", 900.0)], &opts, 0), 0);
    }
}
