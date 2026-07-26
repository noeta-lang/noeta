//! The server/daemon verbs (`lsp`/`dap`/`mcp` — thin stdio launchers) and the profiler
//! entry. (The cache inspector grew into its own module, `cmd::cache`.)

use std::path::PathBuf;
use std::process::ExitCode;

/// Start the Noeta language server over stdio, blocking until the editor client disconnects.
pub(crate) fn cmd_lsp() -> ExitCode {
    noeta_lsp::run_stdio();
    ExitCode::SUCCESS
}

/// Start the Noeta debug adapter over stdio, blocking until the editor client disconnects.
pub(crate) fn cmd_dap() -> ExitCode {
    noeta_dap::run_stdio();
    ExitCode::SUCCESS
}

/// Start the Noeta MCP server over stdio, blocking until the agent client disconnects.
pub(crate) fn cmd_mcp() -> ExitCode {
    noeta_mcp::run_stdio();
    ExitCode::SUCCESS
}

/// Profile a program: run it tier-0 under the production VM and report where it spends its time.
/// Sampling (wall-time flamegraph) is the default; `--instrument` selects the exact per-function
/// profiler; `--every N` makes sampling deterministic (op-weighted).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_profile(
    file: &std::path::Path,
    instrument: bool,
    alloc: bool,
    hz: Option<u32>,
    every: Option<u64>,
    format: Option<&str>,
    out: Option<PathBuf>,
    lines: bool,
    jit: bool,
) -> ExitCode {
    let mode = if instrument {
        noeta_prof::Mode::Instrument
    } else if alloc {
        noeta_prof::Mode::Alloc
    } else if let Some(every) = every {
        // The op-clock cannot see native code (native ops don't advance the counter), so tier-1
        // sampling is a wall-clock concern; `--jit` with `--every` stays tier-0.
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Ops { every },
            lines,
            jit: false,
        }
    } else {
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Wall {
                hz: hz.unwrap_or(1000),
            },
            lines,
            jit,
        }
    };
    let format = match format {
        Some(s) => match noeta_prof::Format::parse(s) {
            Some(f) => Some(f),
            None => {
                eprintln!(
                    "noeta: unknown --format '{s}' (expected folded, svg, speedscope, table, or json)"
                );
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    noeta_prof::run(file, mode, format, out)
}
