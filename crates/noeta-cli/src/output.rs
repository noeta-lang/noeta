//! Shared CLI output helpers: diagnostic rendering to stderr, abort traces, and small
//! formatting utilities every subcommand shares.

use std::io::{self, Write};

use noeta_diagnostics::{Diagnostic, render_colored, render_mapped_colored, stderr_color};
use noeta_span::{Source, SourceMap};

/// The two functions below are the CLI's whole diagnostic funnel — `run`, `build`, `test`, `check`,
/// `serve` and the REPL all print through them — which is why the colour decision is made here and
/// not at each of their call sites. [`stderr_color`] resolves the process's `--color` against
/// stderr, so a pipe or a redirect gets exactly the bytes it always did.
pub(crate) fn emit_diagnostics<'a>(
    source: &Source,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) {
    let color = stderr_color();
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let _ = stderr.write_all(render_colored(source, diagnostic, color).as_bytes());
    }
}

/// Print [`noeta_diagnostics::render_mapped`]'s cross-module rendering to stderr — each diagnostic
/// resolved against the source its span belongs to.
pub(crate) fn emit_diagnostics_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) {
    let _ = io::stderr()
        .write_all(render_mapped_colored(sources, diagnostics, stderr_color()).as_bytes());
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Format a byte count as a short human-readable string (B / KiB / MiB / GiB).
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a session entry's abort trace to stderr, resolving each frame against `map` — the same
/// rendering and "only when there is a real call chain (≥2 frames)" rule `noeta run` uses (a single
/// frame just repeats the diagnostic's own location). With per-entry sources in `map`, a frame from a
/// function defined in an earlier entry now shows that entry's real file and line.
pub(crate) fn emit_trace(trace: &[noeta_vm::TraceFrame], map: &SourceMap) {
    if trace.len() >= 2 {
        eprint!(
            "{}",
            noeta_vm::render_trace_colored(trace, map, stderr_color())
        );
    }
}
