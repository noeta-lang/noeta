//! `noeta-prof` — the built-in dev profiler / flamegraph, the `noeta profile` subcommand's engine.
//!
//! A dev-time introspection tool over the **production bytecode VM**, sibling to `noeta dap` /
//! `noeta lsp` in the dev-tooling cluster. Like the debugger it runs the same `load → check →
//! compile → VM` pipeline as `noeta run` but pins **tier-0** (the JIT unarmed), so every frame is
//! interpreter-executed and observable at an op boundary. Because its signal is wall-time and call
//! structure — not program output — it lives outside the differential oracle (as DAP/LSP do).
//!
//! Two modes are planned: an *instrumenting* profiler (exact per-function call counts + self/total
//! time — **P1, implemented here**) and a *sampling* profiler (wall-time flamegraphs — P2). The
//! collectors ride one per-op seam on the VM ([`noeta_vm::ProfileHook`]); this crate owns the
//! collectors, the `proto → name @ file:line` resolution, and the report rendering.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

mod instrument;
mod session;

/// Which profiler to run. `Summary` just times the run (the P0 baseline); `Instrument` attaches the
/// exact per-function collector. (`Sample` — wall-time flamegraphs — arrives in P2.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Time the whole run, no per-function breakdown.
    Summary,
    /// Exact per-function call counts + self/total time (the instrumenting profiler).
    Instrument,
}

/// One function's resolved profile row: its label + source location and its counters/times.
#[derive(Debug, Clone)]
pub struct FnStat {
    /// The function's name (`"main"`, `"fib"`, `"Point.mag"`), or `"<anonymous>"` for a closure/thunk.
    pub name: String,
    /// The source file the function is defined in, if known.
    pub file: Option<String>,
    /// The 1-based line the function is defined on, if known.
    pub line: Option<u32>,
    /// How many times the function was called (every activation, including recursive).
    pub calls: u64,
    /// Nanoseconds spent in the function's *own* body (excluding callees) — the primary sort key.
    pub self_ns: u64,
    /// Nanoseconds spent in the function *and its callees* (inclusive), counted at the outermost
    /// activation so recursion is not double-counted.
    pub total_ns: u64,
}

/// The outcome of a profiled run, as structured data — the testable core behind [`run`]. The CLI
/// entry replays `stdout`/`stderr` and prints the profile; tests assert on the fields directly.
#[derive(Debug)]
pub struct Report {
    /// The program's own standard output, forwarded verbatim.
    pub stdout: String,
    /// The program's diagnostics + any abort trace (never the profiler's own report).
    pub stderr: String,
    /// The program's exit code.
    pub exit_code: i32,
    /// Wall-clock time the *program* ran (excludes compilation).
    pub wall: Duration,
    /// The per-function breakdown, sorted by self-time descending. `Some` only in [`Mode::Instrument`]
    /// (and empty if the program failed to start).
    pub functions: Option<Vec<FnStat>>,
}

/// Compile the program at `path` tier-0 and run it under the chosen profiler, returning the outcome
/// as a [`Report`] without touching the real streams. A compile/load failure comes back as a
/// `Report` with the diagnostics in `stderr` and a non-zero `exit_code` (never a panic).
pub fn profile(path: &Path, mode: Mode) -> Report {
    let compiled = match session::compile_file(path) {
        Ok(compiled) => compiled,
        Err(out) => return report_from(out, None),
    };

    let hook: Option<Box<dyn noeta_vm::ProfileHook>> = match mode {
        Mode::Summary => None,
        Mode::Instrument => Some(Box::new(instrument::InstrumentCollector::new(
            compiled.module.protos.len(),
        ))),
    };

    let (out, hook) = session::run(&compiled, hook);
    let functions = hook.map(|hook| resolve(hook, &compiled));
    report_from(out, functions)
}

/// Resolve the collector's raw per-proto counters into labelled, self-time-sorted [`FnStat`] rows.
fn resolve(hook: Box<dyn noeta_vm::ProfileHook>, compiled: &session::Compiled) -> Vec<FnStat> {
    let collector = *hook
        .into_any()
        .downcast::<instrument::InstrumentCollector>()
        .expect("the instrument mode installs an InstrumentCollector");

    let mut rows: Vec<FnStat> = collector
        .finish()
        .into_iter()
        .map(|raw| {
            let chunk = &compiled.module.protos[raw.proto as usize];
            let (file, line) = match chunk.def_span {
                Some(span) => (
                    Some(compiled.sources.source(span.source).name().to_string()),
                    Some(compiled.sources.line_col(span).line),
                ),
                None => (None, None),
            };
            FnStat {
                name: chunk
                    .name
                    .clone()
                    .unwrap_or_else(|| "<anonymous>".to_string()),
                file,
                line,
                calls: raw.calls,
                self_ns: raw.self_ns,
                total_ns: raw.total_ns,
            }
        })
        .collect();
    // Hottest self-time first; ties broken by total time, then name for a stable order.
    rows.sort_by(|a, b| {
        b.self_ns
            .cmp(&a.self_ns)
            .then(b.total_ns.cmp(&a.total_ns))
            .then(a.name.cmp(&b.name))
    });
    rows
}

fn report_from(out: session::RunOutput, functions: Option<Vec<FnStat>>) -> Report {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for chunk in out.chunks {
        match chunk.category {
            "stdout" => stdout.push_str(&chunk.text),
            _ => stderr.push_str(&chunk.text),
        }
    }
    Report {
        stdout,
        stderr,
        exit_code: out.exit_code,
        wall: out.wall,
        functions,
    }
}

/// Render the instrumenting profiler's per-function table (a fixed-width text table). Empty string
/// when there is nothing to show.
pub fn render_table(report: &Report) -> String {
    let Some(functions) = &report.functions else {
        return String::new();
    };
    if functions.is_empty() {
        return String::new();
    }
    let total_self: u64 = functions.iter().map(|f| f.self_ns).sum();
    let name_w = functions
        .iter()
        .map(|f| f.name.len())
        .max()
        .unwrap_or(8)
        .clamp(8, 48);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<nw$}  {:>10}  {:>12}  {:>12}  {:>6}\n",
        "function",
        "calls",
        "self",
        "total",
        "self%",
        nw = name_w
    ));
    for f in functions {
        let pct = if total_self > 0 {
            100.0 * f.self_ns as f64 / total_self as f64
        } else {
            0.0
        };
        let loc = match f.line {
            Some(line) => format!("  ({}:{})", f.file.as_deref().unwrap_or("?"), line),
            None => String::new(),
        };
        out.push_str(&format!(
            "{:<nw$}  {:>10}  {:>12}  {:>12}  {:>5.1}%{}\n",
            truncate(&f.name, name_w),
            f.calls,
            fmt_ns(f.self_ns),
            fmt_ns(f.total_ns),
            pct,
            loc,
            nw = name_w
        ));
    }
    out
}

fn truncate(s: &str, w: usize) -> String {
    if s.len() <= w {
        s.to_string()
    } else {
        format!("{}…", &s[..w.saturating_sub(1)])
    }
}

/// Format a nanosecond count as a human duration (`std::time::Duration`'s `{:.3?}` — `1.234ms`, …).
fn fmt_ns(ns: u64) -> String {
    format!("{:.3?}", Duration::from_nanos(ns))
}

/// Profile the program at `path` in `mode`, replay its output on the real streams, and print the
/// profile report. The program's own stdout/stderr are forwarded verbatim; the profiler's report
/// goes to **stderr** so it never mixes into the program's stdout (a piped program stays pipeable).
/// Returns the program's exit code.
pub fn run(path: &Path, mode: Mode) -> ExitCode {
    use std::io::Write;

    let report = profile(path, mode);
    print!("{}", report.stdout);
    let _ = std::io::stdout().flush();
    eprint!("{}", report.stderr);

    let mut err = std::io::stderr();
    match mode {
        Mode::Summary => {
            let _ = writeln!(
                err,
                "noeta profile: program ran in {:.3?} (tier-0)",
                report.wall
            );
        }
        Mode::Instrument => {
            let _ = writeln!(
                err,
                "noeta profile: {} functions, program ran in {:.3?} (tier-0, instrumenting)",
                report.functions.as_ref().map_or(0, |f| f.len()),
                report.wall
            );
            let _ = write!(err, "{}", render_table(&report));
        }
    }
    ExitCode::from(report.exit_code.clamp(0, 255) as u8)
}
