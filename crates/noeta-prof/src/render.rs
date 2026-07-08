//! Emit formats for a profile (P3): the folded stacks P2 produced, an SVG flamegraph (via
//! `inferno`), a speedscope JSON profile, and — for the instrumenting profiler — a JSON table.
//!
//! Everything a profiler emits is *text*, so a format is either written to a `-o <file>` or, without
//! one, to stderr (the program being profiled owns stdout). The folded stacks are the interchange
//! form both the SVG renderer and speedscope build on.

use crate::{Report, render_folded, render_table};

/// The artifact a profile is rendered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Brendan-Gregg collapsed stacks (`main;fib;fib 42`) — sampling.
    Folded,
    /// A self-contained SVG flamegraph (via `inferno`) — sampling.
    Svg,
    /// A speedscope JSON profile (opens at speedscope.app) — sampling.
    Speedscope,
    /// The instrumenting profiler's per-function table (plain text).
    Table,
    /// The instrumenting profiler's per-function rows as JSON.
    Json,
}

impl Format {
    /// Parse a `--format` value. `None`-returning for an unknown token (the CLI reports the error).
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "folded" => Some(Format::Folded),
            "svg" => Some(Format::Svg),
            "speedscope" => Some(Format::Speedscope),
            "table" => Some(Format::Table),
            "json" => Some(Format::Json),
            _ => None,
        }
    }

    /// Whether this format describes a **sampling** artifact (vs. an instrumenting one).
    pub fn is_sampling(self) -> bool {
        matches!(self, Format::Folded | Format::Svg | Format::Speedscope)
    }
}

/// Render `report` in `format`, returning the artifact bytes. An empty vec means "nothing to emit"
/// (e.g. the mode produced no matching data). SVG rendering can fail; that surfaces as an `Err`.
pub fn render(report: &Report, format: Format) -> Result<Vec<u8>, String> {
    match format {
        Format::Folded => Ok(render_folded(report).into_bytes()),
        Format::Table => Ok(render_table(report).into_bytes()),
        Format::Json => Ok(instrument_json(report).into_bytes()),
        Format::Speedscope => Ok(speedscope_json(report).into_bytes()),
        Format::Svg => svg(report).map(String::into_bytes),
    }
}

/// The instrumenting profiler's rows as a JSON array (name, file, line, calls, self_ns, total_ns).
fn instrument_json(report: &Report) -> String {
    let rows: Vec<serde_json::Value> = report
        .functions
        .iter()
        .flatten()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "file": f.file,
                "line": f.line,
                "calls": f.calls,
                "self_ns": f.self_ns,
                "total_ns": f.total_ns,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({ "functions": rows }))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Render the flamegraph to an SVG string via `inferno` (from the folded stacks). Errors if inferno's
/// writer fails or the UTF-8 assembly fails (neither expected for in-memory rendering).
fn svg(report: &Report) -> Result<String, String> {
    let folded = render_folded(report);
    if folded.is_empty() {
        return Ok(String::new());
    }
    let mut opts = inferno::flamegraph::Options::default();
    opts.title = "noeta profile".to_string();
    opts.count_name = "samples".to_string();
    let mut out: Vec<u8> = Vec::new();
    inferno::flamegraph::from_lines(&mut opts, folded.lines(), &mut out)
        .map_err(|e| format!("inferno flamegraph rendering failed: {e}"))?;
    String::from_utf8(out).map_err(|e| format!("flamegraph SVG was not valid UTF-8: {e}"))
}

/// Build a speedscope "sampled" profile (opens at speedscope.app). Each folded stack becomes one
/// weighted sample over a shared frame table; weights are sample counts.
fn speedscope_json(report: &Report) -> String {
    let Some(flame) = &report.flamegraph else {
        return "{}".to_string();
    };

    // Intern frame labels → indices for the shared frame table.
    let mut frame_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut frames: Vec<serde_json::Value> = Vec::new();
    let mut intern = |label: &str| -> usize {
        if let Some(&i) = frame_index.get(label) {
            return i;
        }
        let i = frames.len();
        frames.push(serde_json::json!({ "name": label }));
        frame_index.insert(label.to_string(), i);
        i
    };

    let mut samples: Vec<Vec<usize>> = Vec::new();
    let mut weights: Vec<u64> = Vec::new();
    for stack in &flame.stacks {
        let indices: Vec<usize> = stack.frames.iter().map(|f| intern(f)).collect();
        samples.push(indices);
        weights.push(stack.count);
    }

    let profile = serde_json::json!({
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "version": "0.0.1",
        "exporter": "noeta profile",
        "name": "noeta profile",
        "shared": { "frames": frames },
        "profiles": [{
            "type": "sampled",
            "name": "noeta profile",
            "unit": "none",
            "startValue": 0,
            "endValue": flame.total,
            "samples": samples,
            "weights": weights,
        }],
    });
    serde_json::to_string(&profile).unwrap_or_else(|_| "{}".to_string())
}
