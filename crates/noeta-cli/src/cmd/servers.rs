//! The server/daemon verbs (`lsp`/`dap`/`mcp` — thin stdio launchers), the profiler
//! entry, and the startup-cache inspector.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::CacheAction;
use crate::output::human_bytes;

/// `noeta cache <path|info|clear>` — inspect or clear the transparent startup cache.
pub(crate) fn cmd_cache(action: &CacheAction) -> ExitCode {
    let Some(dir) = noeta_cache::Cache::locate() else {
        eprintln!("noeta: no cache directory could be resolved (set HOME or NOETA_CACHE_DIR)");
        return ExitCode::from(1);
    };
    match action {
        CacheAction::Path => {
            println!("{}", dir.display());
            ExitCode::SUCCESS
        }
        CacheAction::Info => {
            let cap = noeta_cache::max_bytes();
            let cap_str = if cap == 0 {
                "unbounded".to_string()
            } else {
                human_bytes(cap)
            };
            if !dir.exists() {
                println!("{}\n0 entries, 0 B on disk (cap {cap_str})", dir.display());
                return ExitCode::SUCCESS;
            }
            match noeta_cache::Cache::open_at(dir.clone()).and_then(|c| c.stats()) {
                Ok((count, bytes)) => {
                    println!("{}", dir.display());
                    println!(
                        "{count} {}, {} on disk (cap {cap_str})",
                        if count == 1 { "entry" } else { "entries" },
                        human_bytes(bytes),
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("noeta: cannot read cache at {}: {err}", dir.display());
                    ExitCode::from(1)
                }
            }
        }
        CacheAction::Clear => {
            if !dir.exists() {
                println!("cache is already empty ({})", dir.display());
                return ExitCode::SUCCESS;
            }
            match noeta_cache::Cache::open_at(dir.clone()).and_then(|c| c.clear()) {
                Ok(n) => {
                    println!(
                        "removed {n} cached compilation{} from {}",
                        if n == 1 { "" } else { "s" },
                        dir.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("noeta: cannot clear cache at {}: {err}", dir.display());
                    ExitCode::from(1)
                }
            }
        }
    }
}

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
    hz: Option<u32>,
    every: Option<u64>,
    format: Option<&str>,
    out: Option<PathBuf>,
    lines: bool,
) -> ExitCode {
    let mode = if instrument {
        noeta_prof::Mode::Instrument
    } else if let Some(every) = every {
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Ops { every },
            lines,
        }
    } else {
        noeta_prof::Mode::Sample {
            clock: noeta_prof::SampleClock::Wall {
                hz: hz.unwrap_or(1000),
            },
            lines,
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
