//! `noeta-prof` — the built-in dev profiler / flamegraph, the `noeta profile` subcommand's engine.
//!
//! A dev-time introspection tool over the **production bytecode VM**, sibling to `noeta dap` /
//! `noeta lsp` in the dev-tooling cluster. Like the debugger it runs the same `load → check →
//! compile → VM` pipeline as `noeta run` but pins **tier-0** (the JIT unarmed), so every frame is
//! interpreter-executed and observable at an op boundary. Because its signal is wall-time and call
//! structure — not program output — it lives outside the differential oracle (as DAP/LSP do).
//!
//! Two modes: an *instrumenting* profiler (exact per-function call counts + self/total time —
//! `instrument`) and a *sampling* profiler (wall-time or deterministic op-weighted flamegraphs —
//! `sample`). Both collectors ride one per-op seam on the VM ([`noeta_vm::ProfileHook`]); this crate
//! owns the collectors, the `proto → name @ file:line` resolution, and the report rendering (a
//! per-function table, and folded stacks for the flamegraph).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

mod alloc;
mod instrument;
mod render;
mod sample;
mod session;

pub use render::{Format, render};

/// Which profiler to run. `Summary` just times the run; `Instrument` attaches the exact per-function
/// collector; `Sample` builds a wall-time (or, deterministically, op-weighted) flamegraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Time the whole run, no per-function breakdown.
    Summary,
    /// Exact per-function call counts + self/total time (the instrumenting profiler).
    Instrument,
    /// Exact allocated-bytes attribution over the call tree (the memory flamegraph): every byte
    /// the interpreter thread allocates is banked to the executing call path, via the binary's
    /// counting global allocator. Frees are ignored — this answers "who allocates", not "who
    /// retains".
    Alloc,
    /// Periodic stack sampling → a folded-stack flamegraph. `lines` attributes the leaf frame to its
    /// current source line (`fn:line` in the folded labels) rather than just the function.
    Sample { clock: SampleClock, lines: bool },
}

/// What clock drives the sampling profiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleClock {
    /// Real wall-clock ticks at `hz` Hz — the true, nondeterministic profile.
    Wall { hz: u32 },
    /// One sample every `every` executed ops — deterministic and reproducible (for tests / stable
    /// diffs), work-weighted rather than time-weighted.
    Ops { every: u64 },
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
    /// The sampled flamegraph as folded stacks. `Some` only in [`Mode::Sample`].
    pub flamegraph: Option<Flamegraph>,
    /// One additional flamegraph per **worker isolate** the run spawned (real OS threads), in
    /// spawn-finish order, each named `isolate <fn> #<n>`. Same unit as the main flamegraph.
    /// Empty for isolate-free programs and for [`Mode::Summary`].
    pub isolates: Vec<(String, Flamegraph)>,
}

/// What one unit of a flamegraph's stack weight means — sample counts (the sampling profiler) or
/// exact measured nanoseconds (the instrumenting profiler's call tree). Same shape, different unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlameUnit {
    Samples,
    Nanoseconds,
    Bytes,
}

impl FlameUnit {
    /// The short human label for a weight (`42 samples` / `42 ns`).
    pub fn label(self) -> &'static str {
        match self {
            FlameUnit::Samples => "samples",
            FlameUnit::Nanoseconds => "ns",
            FlameUnit::Bytes => "bytes",
        }
    }
    /// The speedscope-schema `unit` string.
    pub fn speedscope(self) -> &'static str {
        match self {
            FlameUnit::Samples => "none",
            FlameUnit::Nanoseconds => "nanoseconds",
            FlameUnit::Bytes => "bytes",
        }
    }
}

/// A flamegraph: a shared table of resolved frames plus every distinct call stack (as index chains
/// into that table), each with its weight — sample counts from the sampling profiler, or exact
/// self-nanoseconds from the instrumenting call tree ([`FlameUnit`]). The same shape speedscope
/// uses, so structured emitters map onto it directly; the folded text derives the labels.
#[derive(Debug, Clone)]
pub struct Flamegraph {
    /// Total weight across the run (samples taken, or nanoseconds measured).
    pub total: u64,
    /// What one unit of weight means.
    pub unit: FlameUnit,
    /// The shared frame table, indexed by [`FoldedStack::frames`]. Ordered by first use across the
    /// sorted stacks, so the table (like the stacks) is deterministic under the op-clock.
    pub frames: Vec<FrameInfo>,
    /// One folded stack per distinct sampled call chain, sorted for a stable (diffable) order.
    pub stacks: Vec<FoldedStack>,
}

impl Flamegraph {
    /// The label chain of a stack, root → leaf — the folded-text view of an index chain.
    pub fn labels<'a>(&'a self, stack: &'a FoldedStack) -> impl Iterator<Item = &'a str> {
        stack
            .frames
            .iter()
            .map(|&i| self.frames[i as usize].label.as_str())
    }
}

/// One resolved frame in the shared table: the folded display label plus its structured source
/// location, so an emitter (or an editor UI) can map the frame back to source without parsing the
/// label. Frame identity is the label: with line attribution the same function appears once per
/// sampled leaf line (`hot:4`, `hot:5`) *and* once bare when it is an interior frame.
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// The folded display label: `fib`, `hot:4` (line-attributed leaf), or `<anonymous>@file:line`.
    pub label: String,
    /// The bare function name (`"fib"`, `"Point.mag"`), or `"<anonymous>"` for a closure/thunk.
    pub name: String,
    /// The source file the frame resolves to, if known.
    pub file: Option<String>,
    /// The 1-based source line: the sampled leaf line for a line-attributed frame, otherwise the
    /// line the function is defined on.
    pub line: Option<u32>,
    /// The 1-based column of the function's definition, if known. `None` for a line-attributed
    /// leaf frame (several pcs merge into one line, so no single column is honest).
    pub col: Option<u32>,
}

/// One folded stack: the chain of frame-table indices root → leaf, and how many samples landed
/// on it.
#[derive(Debug, Clone)]
pub struct FoldedStack {
    /// Indices into [`Flamegraph::frames`] from the outermost (`main`) to the innermost (leaf)
    /// frame.
    pub frames: Vec<u32>,
    /// Sample count for this exact stack.
    pub count: u64,
}

/// Compile the program at `path` tier-0 and run it under the chosen profiler, returning the outcome
/// as a [`Report`] without touching the real streams. A compile/load failure comes back as a
/// `Report` with the diagnostics in `stderr` and a non-zero `exit_code` (never a panic).
pub fn profile(path: &Path, mode: Mode) -> Report {
    let compiled = match session::compile_file(path) {
        Ok(compiled) => compiled,
        Err(out) => return report_from(out, None, None, Vec::new()),
    };

    match mode {
        Mode::Summary => {
            let (out, _) = session::run(&compiled, None, None);
            report_from(out, None, None, Vec::new())
        }
        Mode::Instrument => {
            let hook = Box::new(instrument::InstrumentCollector::new(
                compiled.module.protos.len(),
            ));
            let protos = compiled.module.protos.len();
            let (factory, sink) =
                isolate_seam(move |_| Box::new(instrument::InstrumentCollector::new(protos)));
            let (out, hook) = session::run(&compiled, Some(hook), Some((factory, sink.clone())));
            let (functions, flamegraph) = match hook {
                Some(hook) => {
                    let collector = *hook
                        .into_any()
                        .downcast::<instrument::InstrumentCollector>()
                        .expect("the instrument mode installs an InstrumentCollector");
                    let (stats, tree) = collector.finish();
                    (
                        Some(resolve_functions(stats, &compiled)),
                        Some(resolve_tree_flamegraph(
                            tree,
                            &compiled,
                            FlameUnit::Nanoseconds,
                        )),
                    )
                }
                None => (None, None),
            };
            let isolates = drain_sink(sink, |hook| {
                let collector = *hook
                    .into_any()
                    .downcast::<instrument::InstrumentCollector>()
                    .expect("isolate hooks in instrument mode are InstrumentCollectors");
                let (_, tree) = collector.finish();
                resolve_tree_flamegraph(tree, &compiled, FlameUnit::Nanoseconds)
            });
            report_from(out, functions, flamegraph, isolates)
        }
        Mode::Alloc => {
            let hook = Box::new(alloc::AllocCollector::new());
            let (factory, sink) = isolate_seam(|_| Box::new(alloc::AllocCollector::new()));
            let (out, hook) = session::run(&compiled, Some(hook), Some((factory, sink.clone())));
            let flamegraph = hook.map(|hook| {
                let collector = *hook
                    .into_any()
                    .downcast::<alloc::AllocCollector>()
                    .expect("the alloc mode installs an AllocCollector");
                resolve_tree_flamegraph(collector.finish(), &compiled, FlameUnit::Bytes)
            });
            let isolates = drain_sink(sink, |hook| {
                let collector = *hook
                    .into_any()
                    .downcast::<alloc::AllocCollector>()
                    .expect("isolate hooks in alloc mode are AllocCollectors");
                resolve_tree_flamegraph(collector.finish(), &compiled, FlameUnit::Bytes)
            });
            report_from(out, None, flamegraph, isolates)
        }
        Mode::Sample { clock, lines } => {
            // Wall-clock sampling needs a timer thread bumping per-collector atomics (a fanout, so
            // every isolate's collector ticks too); op-clock is self-contained. Either way the
            // collectors ride the per-op seam and come back with their aggregated stacks.
            let (collector, timer, fanout) = match clock {
                SampleClock::Wall { hz } => {
                    let pending = Arc::new(AtomicU32::new(0));
                    let fanout = Arc::new(std::sync::Mutex::new(vec![Arc::clone(&pending)]));
                    let timer = sample::spawn_timer(hz, Arc::clone(&fanout));
                    (
                        sample::SampleCollector::wall(pending, lines),
                        Some(timer),
                        Some(fanout),
                    )
                }
                SampleClock::Ops { every } => {
                    (sample::SampleCollector::ops(every, lines), None, None)
                }
            };
            let (factory, sink) = isolate_seam(move |_| -> Box<dyn noeta_vm::ProfileHook> {
                match (clock, &fanout) {
                    (SampleClock::Wall { .. }, Some(fanout)) => {
                        // Register this isolate's tick target with the shared timer.
                        let pending = Arc::new(AtomicU32::new(0));
                        if let Ok(mut targets) = fanout.lock() {
                            targets.push(Arc::clone(&pending));
                        }
                        Box::new(sample::SampleCollector::wall(pending, lines))
                    }
                    (SampleClock::Ops { every }, _) => {
                        Box::new(sample::SampleCollector::ops(every, lines))
                    }
                    // Unreachable: wall clock always builds a fanout above.
                    (SampleClock::Wall { .. }, None) => unreachable!("wall clock has a fanout"),
                }
            });
            let (out, hook) = session::run(
                &compiled,
                Some(Box::new(collector)),
                Some((factory, sink.clone())),
            );
            // Stop the timer *before* resolving, so no further ticks accrue.
            if let Some(timer) = timer {
                timer.stop();
            }
            let flamegraph = hook.map(|hook| resolve_flamegraph(hook, &compiled));
            let isolates = drain_sink(sink, |hook| resolve_flamegraph(hook, &compiled));
            report_from(out, None, flamegraph, isolates)
        }
    }
}

/// Build the per-isolate profile seam: a hook factory from `make` plus the sink finished workers
/// deposit into. The factory receives the isolate's display name (unused by the current
/// collectors, forwarded for future ones).
fn isolate_seam(
    make: impl Fn(&str) -> Box<dyn noeta_vm::ProfileHook> + Send + Sync + 'static,
) -> (noeta_vm::ProfileHookFactory, noeta_vm::ProfileSink) {
    let sink: noeta_vm::ProfileSink = Arc::new(std::sync::Mutex::new(Vec::new()));
    (Arc::new(make), sink)
}

/// Drain the isolate sink, resolving each named hook into its flamegraph via `resolve`.
fn drain_sink(
    sink: noeta_vm::ProfileSink,
    mut resolve: impl FnMut(Box<dyn noeta_vm::ProfileHook>) -> Flamegraph,
) -> Vec<(String, Flamegraph)> {
    let mut drained = match sink.lock() {
        Ok(mut sink) => std::mem::take(&mut *sink),
        Err(_) => Vec::new(),
    };
    drained
        .drain(..)
        .map(|(name, hook)| (name, resolve(hook)))
        .collect()
}

/// Resolve a prototype to its [`FrameInfo`]: name + definition site, and the display label — the
/// function name, or `<anonymous>@file:line` for a nameless closure/thunk (so distinct anonymous
/// frames stay distinguishable in a flamegraph).
fn proto_frame(compiled: &session::Compiled, proto: u32) -> FrameInfo {
    let chunk = &compiled.module.protos[proto as usize];
    let (file, line, col) = match chunk.def_span {
        Some(span) => {
            let lc = compiled.sources.line_col(span);
            (
                Some(compiled.sources.source(span.source).name().to_string()),
                Some(lc.line),
                Some(lc.col),
            )
        }
        None => (None, None, None),
    };
    let name = chunk
        .name
        .clone()
        .unwrap_or_else(|| "<anonymous>".to_string());
    let label = match (&chunk.name, &file, line) {
        (Some(name), ..) => name.clone(),
        (None, Some(file), Some(line)) => format!("<anonymous>@{file}:{line}"),
        (None, ..) => name.clone(),
    };
    FrameInfo {
        label,
        name,
        file,
        line,
        col,
    }
}

/// Resolve a captured leaf `pc` (in prototype `proto`) to a 1-based source line, via the
/// prototype's always-emitted line table. `None` if the pc predates the first spanned statement.
fn leaf_line(compiled: &session::Compiled, proto: u32, pc: usize) -> Option<u32> {
    let span = compiled.module.protos[proto as usize].line_span(pc)?;
    Some(compiled.sources.line_col(span).line)
}

/// The top `n` functions by leaf (self) samples: the innermost frame of each sampled stack is that
/// stack's "current" function, so aggregating stacks by their leaf label gives a quick "where is the
/// program *actually* executing" view — the human summary complementing the full folded artifact.
/// Returns `(label, samples, percent)` heaviest-first. Empty when there is no flamegraph.
pub fn top_functions(report: &Report, n: usize) -> Vec<(String, u64, f64)> {
    let Some(flame) = &report.flamegraph else {
        return Vec::new();
    };
    // Frame identity is the table index (one per label), so aggregating by index is aggregating
    // by label.
    let mut by_leaf: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    for stack in &flame.stacks {
        if let Some(&leaf) = stack.frames.last() {
            *by_leaf.entry(leaf).or_insert(0) += stack.count;
        }
    }
    let mut rows: Vec<(String, u64, f64)> = by_leaf
        .into_iter()
        .map(|(leaf, samples)| {
            let label = &flame.frames[leaf as usize].label;
            let pct = if flame.total > 0 {
                100.0 * samples as f64 / flame.total as f64
            } else {
                0.0
            };
            (label.to_string(), samples, pct)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(n);
    rows
}

/// Resolve the collector's raw per-proto counters into labelled, self-time-sorted [`FnStat`] rows.
fn resolve_functions(stats: Vec<instrument::RawStat>, compiled: &session::Compiled) -> Vec<FnStat> {
    let mut rows: Vec<FnStat> = stats
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

/// Resolve the sampler's raw proto-chain counts into labelled folded stacks, sorted for a stable
/// (diffable, op-clock-reproducible) order.
fn resolve_flamegraph(
    hook: Box<dyn noeta_vm::ProfileHook>,
    compiled: &session::Compiled,
) -> Flamegraph {
    let collector = *hook
        .into_any()
        .downcast::<sample::SampleCollector>()
        .expect("the sample mode installs a SampleCollector");
    let (total, raw) = collector.finish();

    // Resolve each raw stack to frames, interning by *label*, then re-aggregate by the interned
    // chain: in line-attribution mode several distinct leaf pcs on the same source line resolve to
    // the same `fn:line` label, so they must merge into one folded entry (without lines the keys
    // are already unique per chain, so this is a no-op).
    let mut frame_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut frames: Vec<FrameInfo> = Vec::new();
    let mut intern = |frame: FrameInfo| -> u32 {
        if let Some(&i) = frame_index.get(&frame.label) {
            return i;
        }
        let i = frames.len() as u32;
        frame_index.insert(frame.label.clone(), i);
        frames.push(frame);
        i
    };
    let mut merged: std::collections::HashMap<Vec<u32>, u64> = std::collections::HashMap::new();
    for folded in raw {
        let mut chain: Vec<FrameInfo> = folded
            .chain
            .iter()
            .map(|&proto| proto_frame(compiled, proto))
            .collect();
        if folded.leaf_pc != 0
            && let (Some(&leaf_proto), Some(last)) = (folded.chain.last(), chain.last_mut())
            && let Some(line) = leaf_line(compiled, leaf_proto, folded.leaf_pc as usize)
        {
            last.label.push_str(&format!(":{line}"));
            last.line = Some(line);
            last.col = None;
        }
        let indices: Vec<u32> = chain.into_iter().map(&mut intern).collect();
        *merged.entry(indices).or_insert(0) += folded.count;
    }

    finalize_flamegraph(total, FlameUnit::Samples, frames, merged)
}

/// Resolve the instrumenting collector's exact call tree into a [`Flamegraph`] weighted by
/// measured self-nanoseconds: each tree node with self-time becomes one folded stack (its
/// root→node proto path), so the flamegraph is exact — every call accounted, no sampling error.
fn resolve_tree_flamegraph(
    tree: Vec<instrument::RawTreeNode>,
    compiled: &session::Compiled,
    unit: FlameUnit,
) -> Flamegraph {
    let mut frame_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut frames: Vec<FrameInfo> = Vec::new();
    let mut intern = |frame: FrameInfo| -> u32 {
        if let Some(&i) = frame_index.get(&frame.label) {
            return i;
        }
        let i = frames.len() as u32;
        frame_index.insert(frame.label.clone(), i);
        frames.push(frame);
        i
    };
    // Each node's proto path, built parents-first (a parent always precedes its children in the
    // trie's append order, so its path is already computed).
    let mut paths: Vec<Vec<u32>> = Vec::with_capacity(tree.len());
    for node in &tree {
        let mut path = match node.parent {
            Some(parent) => paths[parent as usize].clone(),
            None => Vec::new(),
        };
        path.push(node.proto);
        paths.push(path);
    }
    let mut total = 0u64;
    let mut merged: std::collections::HashMap<Vec<u32>, u64> = std::collections::HashMap::new();
    for (node, path) in tree.iter().zip(&paths) {
        if node.weight == 0 {
            continue; // an interior path with no own weight — its children carry it
        }
        total += node.weight;
        let indices: Vec<u32> = path
            .iter()
            .map(|&proto| intern(proto_frame(compiled, proto)))
            .collect();
        // Distinct paths can share a label chain (e.g. two anonymous protos resolving to one
        // label), so merge by the interned chain exactly as the sampler does.
        *merged.entry(indices).or_insert(0) += node.weight;
    }
    finalize_flamegraph(total, unit, frames, merged)
}

/// Order and pack an assembled flamegraph: heaviest stacks first (label-chain tiebreak for a
/// deterministic order), then the frame table renumbered in first-use order over the sorted
/// stacks, so the artifact is diffable (not hash-iteration-ordered).
fn finalize_flamegraph(
    total: u64,
    unit: FlameUnit,
    frames: Vec<FrameInfo>,
    merged: std::collections::HashMap<Vec<u32>, u64>,
) -> Flamegraph {
    let mut stacks: Vec<FoldedStack> = merged
        .into_iter()
        .map(|(frames, count)| FoldedStack { frames, count })
        .collect();
    stacks.sort_by(|a, b| {
        b.count.cmp(&a.count).then_with(|| {
            let la: Vec<&str> = a
                .frames
                .iter()
                .map(|&i| frames[i as usize].label.as_str())
                .collect();
            let lb: Vec<&str> = b
                .frames
                .iter()
                .map(|&i| frames[i as usize].label.as_str())
                .collect();
            la.cmp(&lb)
        })
    });

    let mut renumber: Vec<Option<u32>> = vec![None; frames.len()];
    let mut ordered: Vec<FrameInfo> = Vec::with_capacity(frames.len());
    for stack in &mut stacks {
        for idx in &mut stack.frames {
            let new = *renumber[*idx as usize].get_or_insert_with(|| {
                ordered.push(frames[*idx as usize].clone());
                (ordered.len() - 1) as u32
            });
            *idx = new;
        }
    }

    Flamegraph {
        total,
        unit,
        frames: ordered,
        stacks,
    }
}

/// The summary-line suffix naming how many isolate profiles the run produced (empty when none).
fn isolates_suffix(report: &Report) -> String {
    match report.isolates.len() {
        0 => String::new(),
        1 => " + 1 isolate profile".to_string(),
        n => format!(" + {n} isolate profiles"),
    }
}

fn report_from(
    out: session::RunOutput,
    functions: Option<Vec<FnStat>>,
    flamegraph: Option<Flamegraph>,
    isolates: Vec<(String, Flamegraph)>,
) -> Report {
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
        flamegraph,
        isolates,
    }
}

/// Render a flamegraph as **folded stacks** — the Brendan-Gregg collapsed format
/// (`main;fib;fib <count>`), one line per stack, which `inferno`/`flamegraph.pl`/speedscope consume.
/// Empty when there is no flamegraph (or no samples).
pub fn render_folded(report: &Report) -> String {
    let Some(flame) = &report.flamegraph else {
        return String::new();
    };
    let mut out = String::new();
    for stack in &flame.stacks {
        out.push_str(&flame.labels(stack).collect::<Vec<_>>().join(";"));
        out.push(' ');
        out.push_str(&stack.count.to_string());
        out.push('\n');
    }
    // Isolate profiles follow, each stack rooted at the isolate's display name — the standard
    // folded convention for threads, so inferno renders one flame per isolate beside `main`'s.
    for (name, flame) in &report.isolates {
        for stack in &flame.stacks {
            out.push_str(name);
            out.push(';');
            out.push_str(&flame.labels(stack).collect::<Vec<_>>().join(";"));
            out.push(' ');
            out.push_str(&stack.count.to_string());
            out.push('\n');
        }
    }
    out
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
/// The default emit format for a mode: `Table` for instrumenting, `Folded` for sampling, and none
/// for the bare timed run.
fn default_format(mode: Mode) -> Option<Format> {
    match mode {
        Mode::Summary => None,
        Mode::Instrument => Some(Format::Table),
        // Sampling's (and alloc's) default human view is the top-N summary printed by `run`, not a
        // folded dump; a machine artifact is emitted only when `--format` is given explicitly.
        Mode::Sample { .. } | Mode::Alloc => None,
    }
}

/// Whether `format` is valid for `mode` (a sampling artifact needs a sampling run, and vice versa).
fn format_fits(mode: Mode, format: Format) -> bool {
    match mode {
        Mode::Sample { .. } => format.is_sampling(),
        // The instrumenting run carries the exact call tree, so the stack-shaped formats
        // (folded/svg/speedscope) render from it too — every format fits.
        Mode::Instrument => true,
        // The alloc run is stack-shaped only (no function table).
        Mode::Alloc => format.is_sampling(),
        Mode::Summary => false,
    }
}

/// Returns the program's exit code. `format` overrides the mode's default artifact; `out` writes that
/// artifact to a file (otherwise it goes to stderr — the program owns stdout).
pub fn run(path: &Path, mode: Mode, format: Option<Format>, out: Option<PathBuf>) -> ExitCode {
    use std::io::Write;

    // Reject a format that doesn't match the mode *before* running the program.
    if let Some(format) = format
        && !format_fits(mode, format)
    {
        eprintln!(
            "noeta profile: --format {format:?} does not apply to this mode (table/json need \
             --instrument)"
        );
        return ExitCode::from(2);
    }

    let report = profile(path, mode);
    print!("{}", report.stdout);
    let _ = std::io::stdout().flush();
    eprint!("{}", report.stderr);

    let mut err = std::io::stderr();
    // The one-line summary always goes to stderr.
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
                "noeta profile: {} functions{}, program ran in {:.3?} (tier-0, instrumenting)",
                report.functions.as_ref().map_or(0, |f| f.len()),
                isolates_suffix(&report),
                report.wall
            );
        }
        Mode::Alloc => {
            let flame = report.flamegraph.as_ref();
            let _ = writeln!(
                err,
                "noeta profile: {} bytes allocated over {} stacks{}, program ran in {:.3?} \
                 (tier-0, alloc)",
                flame.map_or(0, |f| f.total),
                flame.map_or(0, |f| f.stacks.len()),
                isolates_suffix(&report),
                report.wall,
            );
            if flame.map_or(0, |f| f.total) == 0 {
                let _ = writeln!(
                    err,
                    "noeta profile: no allocation counter present — the alloc profile needs the \
                     stock `noeta` binary (its global allocator counts bytes); a composed \
                     toolchain without it reports zero"
                );
            }
        }
        Mode::Sample { clock, .. } => {
            let flame = report.flamegraph.as_ref();
            let clock_desc = match clock {
                SampleClock::Wall { hz } => format!("wall-clock {hz} Hz"),
                SampleClock::Ops { every } => format!("op-clock 1/{every}"),
            };
            let _ = writeln!(
                err,
                "noeta profile: {} samples over {} stacks{}, program ran in {:.3?} \
                 (tier-0, sampling, {clock_desc})",
                flame.map_or(0, |f| f.total),
                flame.map_or(0, |f| f.stacks.len()),
                isolates_suffix(&report),
                report.wall
            );
            // The default human view: the hottest functions by leaf (self) samples.
            let top = top_functions(&report, 10);
            if !top.is_empty() {
                let width = top
                    .iter()
                    .map(|(l, ..)| l.len())
                    .max()
                    .unwrap_or(8)
                    .clamp(8, 48);
                for (label, samples, pct) in top {
                    let _ = writeln!(err, "  {label:<width$}  {samples:>8}  {pct:>5.1}%");
                }
            }
        }
    }

    // Render + emit the artifact (unless the run failed to start or the mode has none).
    if let Some(format) = format.or_else(|| default_format(mode)) {
        match render::render(&report, format) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => match &out {
                // `-o -`: the artifact goes to stdout — for piping into another tool (or an editor
                // integration capturing the artifact directly). It follows the program's own
                // forwarded stdout, so this suits programs that print little or nothing.
                Some(path) if path.as_os_str() == "-" => {
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(&bytes);
                    let _ = stdout.flush();
                }
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &bytes) {
                        let _ =
                            writeln!(err, "noeta profile: cannot write {}: {e}", path.display());
                        return ExitCode::from(2);
                    }
                    let _ = writeln!(
                        err,
                        "noeta profile: wrote {:?} profile to {}",
                        format,
                        path.display()
                    );
                }
                // No file: the artifact goes to stderr (keeping the program's stdout clean).
                None => {
                    let _ = err.write_all(&bytes);
                }
            },
            Err(e) => {
                let _ = writeln!(err, "noeta profile: {e}");
                return ExitCode::from(2);
            }
        }
    }

    ExitCode::from(report.exit_code.clamp(0, 255) as u8)
}
