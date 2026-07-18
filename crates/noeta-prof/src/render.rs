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

    /// Whether this format is stack-shaped (folded/svg/speedscope). Historically "sampling", but
    /// the instrumenting profiler now carries an exact call tree, so these render in either mode.
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

/// The instrumenting profiler's artifact: the per-function rows, plus — when the run carried the
/// exact call tree — the speedscope-shaped stacks alongside (`shared`/`profiles`), so one artifact
/// serves both the function table and an exact flamegraph (the VS Code profile view renders both).
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
    let mut artifact = serde_json::json!({ "functions": rows });
    if let Some(body) = speedscope_value(report) {
        artifact["$schema"] = body["$schema"].clone();
        artifact["shared"] = body["shared"].clone();
        artifact["profiles"] = body["profiles"].clone();
    }
    serde_json::to_string_pretty(&artifact).unwrap_or_else(|_| "{}".to_string())
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
    opts.count_name = report
        .flamegraph
        .as_ref()
        .map(|f| f.unit.label().to_string())
        .unwrap_or_else(|| "samples".to_string());
    let mut out: Vec<u8> = Vec::new();
    inferno::flamegraph::from_lines(&mut opts, folded.lines(), &mut out)
        .map_err(|e| format!("inferno flamegraph rendering failed: {e}"))?;
    String::from_utf8(out).map_err(|e| format!("flamegraph SVG was not valid UTF-8: {e}"))
}

/// Build a speedscope "sampled" profile (opens at speedscope.app). The flamegraph's shared frame
/// table maps directly onto speedscope's: each frame carries its display label as `name` plus the
/// structured `file`/`line`/`col` (speedscope-schema fields) so a consumer — the VS Code profile
/// view — can jump to source without parsing labels. Each folded stack becomes one weighted sample.
fn speedscope_json(report: &Report) -> String {
    match speedscope_value(report) {
        Some(v) => serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

/// The speedscope profile as a JSON value, or `None` when the report has no flamegraph. The
/// weight `unit` comes from the flamegraph ("none" for sample counts, "nanoseconds" for the
/// instrumenting call tree — speedscope and the VS Code view then format weights as time).
fn speedscope_value(report: &Report) -> Option<serde_json::Value> {
    let main = report.flamegraph.as_ref()?;

    // The speedscope container holds one frame table shared by EVERY profile, so the main run's
    // frames and each isolate's are interned together (by label — the same identity the resolvers
    // use) and each profile's stacks are remapped into the combined table.
    let mut frame_index: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut frames: Vec<serde_json::Value> = Vec::new();
    let all: Vec<(&str, &crate::Flamegraph)> = std::iter::once(("main", main))
        .chain(report.isolates.iter().map(|(n, f)| (n.as_str(), f)))
        .collect();
    let mut profiles: Vec<serde_json::Value> = Vec::new();
    for (name, flame) in all {
        let remap: Vec<u32> = flame
            .frames
            .iter()
            .map(|f| {
                if let Some(&i) = frame_index.get(f.label.as_str()) {
                    return i;
                }
                let mut frame = serde_json::json!({ "name": f.label });
                if let Some(file) = &f.file {
                    frame["file"] = serde_json::json!(file);
                }
                if let Some(line) = f.line {
                    frame["line"] = serde_json::json!(line);
                }
                if let Some(col) = f.col {
                    frame["col"] = serde_json::json!(col);
                }
                let i = frames.len() as u32;
                frames.push(frame);
                i
            })
            .collect();
        for (local_idx, f) in flame.frames.iter().enumerate() {
            frame_index
                .entry(f.label.as_str())
                .or_insert(remap[local_idx]);
        }
        let samples: Vec<Vec<u32>> = flame
            .stacks
            .iter()
            .map(|s| s.frames.iter().map(|&i| remap[i as usize]).collect())
            .collect();
        let weights: Vec<u64> = flame.stacks.iter().map(|s| s.count).collect();
        profiles.push(serde_json::json!({
            "type": "sampled",
            "name": name,
            "unit": flame.unit.speedscope(),
            "startValue": 0,
            "endValue": flame.total,
            "samples": samples,
            "weights": weights,
        }));
    }

    let profile = serde_json::json!({
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "version": "0.0.1",
        "exporter": "noeta profile",
        "name": "noeta profile",
        "shared": { "frames": frames },
        "profiles": profiles,
    });
    Some(profile)
}
