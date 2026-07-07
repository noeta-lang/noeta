//! `noeta-prof` — the built-in dev profiler / flamegraph, the `noeta profile` subcommand's engine.
//!
//! A dev-time introspection tool over the **production bytecode VM**, sibling to `noeta dap` /
//! `noeta lsp` in the dev-tooling cluster. Like the debugger it runs the same `load → check →
//! compile → VM` pipeline as `noeta run` but pins **tier-0** (the JIT unarmed), so every frame is
//! interpreter-executed and observable at an op boundary. Because its signal is wall-time and call
//! structure — not program output — it lives outside the differential oracle (as DAP/LSP do).
//!
//! Two modes are planned: an *instrumenting* profiler (exact per-function call counts + self/total
//! time) and a *sampling* profiler (wall-time flamegraphs). **This is P0**: the crate skeleton, the
//! `noeta profile` subcommand, and a tier-0 timed run — the run architecture the collectors bolt
//! onto. No collector is attached yet; the profiler reports only the run's wall-clock time.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

mod session;

/// The outcome of a profiled run, as structured data — the testable core behind [`run`]. The CLI
/// entry replays `stdout`/`stderr` on the real streams and prints the profile; tests assert on the
/// fields directly.
#[derive(Debug)]
pub struct Report {
    /// The program's own standard output, forwarded verbatim.
    pub stdout: String,
    /// The program's diagnostics + any abort trace (never the profiler's own report).
    pub stderr: String,
    /// The program's exit code.
    pub exit_code: i32,
    /// Wall-clock time the *program* ran (excludes compilation) — P0's profiling signal.
    pub wall: Duration,
}

/// Compile the program at `path` tier-0 and run it on the real host, returning the outcome as a
/// [`Report`] without touching the real streams. A compile/load failure comes back as a `Report`
/// with the diagnostics in `stderr` and a non-zero `exit_code` (never a panic).
pub fn profile(path: &Path) -> Report {
    let compiled = match session::compile_file(path) {
        Ok(compiled) => compiled,
        Err(out) => return report_from(out),
    };
    report_from(session::run_compiled(compiled))
}

fn report_from(out: session::RunOutput) -> Report {
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
    }
}

/// Profile the program at `path`, replay its output on the real streams, and print the profile
/// report. The program's own stdout/stderr are forwarded verbatim; the profiler's report goes to
/// **stderr** so it never mixes into the program's stdout (a piped program stays pipeable). Returns
/// the program's exit code.
pub fn run(path: &Path) -> ExitCode {
    use std::io::Write;

    let report = profile(path);
    print!("{}", report.stdout);
    let _ = std::io::stdout().flush();
    eprint!("{}", report.stderr);
    // P0's profile: the run's wall-clock time. Later slices replace/extend this with the
    // per-function table (instrumenting) and the flamegraph (sampling).
    let _ = writeln!(
        std::io::stderr(),
        "noeta profile: program ran in {:.3?} (tier-0)",
        report.wall
    );
    ExitCode::from(report.exit_code.clamp(0, 255) as u8)
}
