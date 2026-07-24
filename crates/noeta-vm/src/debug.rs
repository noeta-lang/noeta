//! Protocol-neutral debug-session support (MCP arc M6, extracted from `noeta-dap`): the pieces
//! every interactive debugger over the [`Debugger`](crate::Debugger) seam needs, independent of
//! the wire it answers on — `noeta dap` (DAP JSON) and `noeta mcp` (MCP tools) both build on this
//! module, so breakpoints resolve, steps land, and stacks capture identically in either.
//!
//! The VM knows only prototypes and program counters; tooling speaks files and lines. The
//! compiler's debug **line table** (`Chunk::line_table`, one `(pc, span)` per source statement)
//! bridges them: [`resolve_breakpoints`] maps requested lines to instruction positions, and at run
//! time it also gives the *current* line for any pc — so stepping ([`StepState`]) and the captured
//! stack ([`capture`]) resolve a line even for an instruction whose own op is spanless (a bare
//! `return x`).
//!
//! Everything here is checker-free (the VM's rule) and renders values to owned strings inside the
//! run thread (`Value` is not `Send` — nothing VM-internal crosses a thread boundary).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use noeta_bytecode::Module;
use noeta_span::{SourceMap, Span};

use crate::DebugView;

/// Resolve editor breakpoint requests (`path → 1-based lines`) against the compiled program into the
/// set of `(proto, pc)` instruction positions the VM should stop at: the first instruction of each
/// requested line, per prototype. Driven off the debug **line table** (`Chunk::line_table`, one entry
/// per source statement in `pc` order), so a line resolves even when its statement compiled to only
/// spanless ops (a bare `return x`). A line with no entry simply yields nothing (the breakpoint is
/// unverifiable — it lands on a blank/comment line or code that compiled away).
pub fn resolve_breakpoints(
    module: &Module,
    sources: &SourceMap,
    requested: &HashMap<String, Vec<u32>>,
) -> HashSet<(u32, usize)> {
    let mut stops = HashSet::new();
    for (proto_idx, chunk) in module.protos.iter().enumerate() {
        // Entries are in `pc` order, so the first entry for a (file, line) is where that line's
        // execution begins in this prototype.
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for entry in &chunk.line_table {
            let source = sources.source(entry.span.source);
            let line = source.line_col(entry.span.start).line;
            if !line_requested(requested, source.name(), line) {
                continue;
            }
            if seen.insert((entry.span.source.0, line)) {
                stops.insert((proto_idx as u32, entry.pc as usize));
            }
        }
    }
    stops
}

/// One editor breakpoint request carrying its optional DAP `condition` and `hitCondition`
/// expressions (conditional / hit-count breakpoints). The strings are opaque here — the wire
/// adapter compiles a `condition` to a console fragment and parses a `hitCondition` into a
/// hit-count matcher; this module only carries them from a requested line to the resolved
/// `(proto, pc)` so the adapter can attach them to the right instruction.
#[derive(Debug, Clone)]
pub struct RequestedBreakpoint {
    /// The 1-based source line the client asked to break on.
    pub line: u32,
    /// A DAP `condition`: an expression evaluated in the paused frame; the breakpoint only stops
    /// when it is true. `None` for an unconditional breakpoint.
    pub condition: Option<String>,
    /// A DAP `hitCondition`: a hit-count expression (`N`, `>N`, `%N`, …); the breakpoint stops only
    /// on hits the expression admits. `None` for every-hit.
    pub hit_condition: Option<String>,
}

/// A breakpoint resolved to a `(proto, pc)` instruction position, carrying the `condition` /
/// `hitCondition` the requesting line asked for (conditional / hit-count breakpoints).
#[derive(Debug, Clone)]
pub struct ResolvedBreakpoint {
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

/// Resolve editor breakpoint requests **with their conditions** (conditional / hit-count
/// breakpoints) into a map from each `(proto, pc)` instruction position to the `condition` /
/// `hitCondition` of the line it resolves to. Same line-table resolution as [`resolve_breakpoints`]
/// (the first pc of each requested line, per prototype), but keyed so the wire adapter can attach a
/// per-breakpoint condition and hit-count matcher. A line that resolves in several prototypes (e.g.
/// a generic instantiated twice) attaches the same condition to each — the natural behavior.
pub fn resolve_conditional_breakpoints(
    module: &Module,
    sources: &SourceMap,
    requested: &HashMap<String, Vec<RequestedBreakpoint>>,
) -> HashMap<(u32, usize), ResolvedBreakpoint> {
    let mut stops = HashMap::new();
    for (proto_idx, chunk) in module.protos.iter().enumerate() {
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for entry in &chunk.line_table {
            let source = sources.source(entry.span.source);
            let line = source.line_col(entry.span.start).line;
            let Some(bp) = requested_breakpoint(requested, source.name(), line) else {
                continue;
            };
            if seen.insert((entry.span.source.0, line)) {
                stops.insert(
                    (proto_idx as u32, entry.pc as usize),
                    ResolvedBreakpoint {
                        condition: bp.condition.clone(),
                        hit_condition: bp.hit_condition.clone(),
                    },
                );
            }
        }
    }
    stops
}

/// The requested breakpoint on `line` of the file named `source_name`, if any — the conditional
/// twin of [`line_requested`], returning the matched [`RequestedBreakpoint`] so its condition
/// travels to the resolved stop.
fn requested_breakpoint<'a>(
    requested: &'a HashMap<String, Vec<RequestedBreakpoint>>,
    source_name: &str,
    line: u32,
) -> Option<&'a RequestedBreakpoint> {
    requested.iter().find_map(|(path, bps)| {
        if path_matches(path, source_name) {
            bps.iter().find(|bp| bp.line == line)
        } else {
            None
        }
    })
}

/// Every `(proto, pc)` that begins a source statement — the instruction positions a breakpoint can
/// resolve to and a landed step stops on, taken straight from each prototype's debug **line table**
/// ([`Chunk::line_table`], one entry per statement).
///
/// These are the only pcs at which a pause sees every in-scope local **materialized** in its
/// register. Mid-statement, a named local can be caught in the window between the `Drop` that frees
/// its old value and the `Move` that writes its new one — its pinned register momentarily reads
/// `unit`. Breakpoints and steps never land off a statement start, so they never observe this; a
/// resume-budget (`limit`) pause, which would otherwise trip at an arbitrary op, defers to the next
/// member of this set for exactly the same guarantee.
pub fn statement_starts(module: &Module) -> HashSet<(u32, usize)> {
    let mut starts = HashSet::new();
    for (proto_idx, chunk) in module.protos.iter().enumerate() {
        for entry in &chunk.line_table {
            starts.insert((proto_idx as u32, entry.pc as usize));
        }
    }
    starts
}

/// Whether `line` in the file named `source_name` is a requested breakpoint. Matches the editor's
/// path against the compiler's source name exactly, by canonical path, or (last resort) by file name.
fn line_requested(requested: &HashMap<String, Vec<u32>>, source_name: &str, line: u32) -> bool {
    requested
        .iter()
        .any(|(path, lines)| path_matches(path, source_name) && lines.contains(&line))
}

/// Whether an editor-supplied `path` refers to the same file as the compiler's `source_name`.
fn path_matches(path: &str, source_name: &str) -> bool {
    if path == source_name {
        return true;
    }
    let (a, b) = (Path::new(path), Path::new(source_name));
    if a.canonicalize()
        .ok()
        .zip(b.canonicalize().ok())
        .is_some_and(|(ca, cb)| ca == cb)
    {
        return true;
    }
    a.file_name().is_some() && a.file_name() == b.file_name()
}

/// A source-line step, as `next` / `stepIn` / `stepOut` ask for it. Stepping is line-granular (not
/// instruction-granular): a step runs until the *source line* changes in a way the mode allows, so
/// one press advances one visible line rather than one bytecode op.
#[derive(Clone, Copy, Debug)]
pub enum StepMode {
    /// `next`: run to the next line in the current frame, running any call the line makes to
    /// completion without stopping inside it (a deeper frame is skipped over).
    Over,
    /// `stepIn`: run to the next line at *any* depth — descend into a call the current line makes.
    Into,
    /// `stepOut`: run until the current frame returns, stopping in the caller.
    Out,
}

/// A step in progress, armed (from the pause it was launched at) when the debugger resumes with a
/// step. `origin_depth` is the call-stack depth there, `origin_line` its innermost frame's
/// `(source, line)`; [`StepState::landed`] compares each instruction against these to decide when
/// the step is done.
#[derive(Debug)]
pub struct StepState {
    mode: StepMode,
    origin_depth: usize,
    origin_line: Option<(u32, u32)>,
}

impl StepState {
    /// Arm a step relative to the current pause point: the next stop is measured against *this*
    /// depth and line.
    pub fn arm(mode: StepMode, view: &DebugView, sources: &SourceMap) -> StepState {
        StepState {
            mode,
            origin_depth: view.depth(),
            origin_line: top_line(view, sources),
        }
    }

    /// Whether the step has landed at this instruction — see [`StepMode`].
    ///
    /// Stepping is line-granular, so a step only ever lands on an instruction that *maps to a
    /// source line*: the many spanless ops (stores, moves, a call's return slot) are stepped
    /// through transparently, which also prevents landing on the resume instruction after a call
    /// (it has no span, so it would otherwise read as line 0).
    pub fn landed(&self, view: &DebugView, sources: &SourceMap) -> bool {
        let Some(line) = top_line(view, sources) else {
            return false;
        };
        let line = Some(line);
        let depth = view.depth();
        match self.mode {
            // Landed once we are shallower than where we started (the frame returned).
            StepMode::Out => depth < self.origin_depth,
            // A new line in the starting frame, or a return past it — but never inside a deeper call.
            StepMode::Over => {
                depth < self.origin_depth
                    || (depth == self.origin_depth && line != self.origin_line)
            }
            // The first instruction at any different (depth, line) — so entering a call lands too.
            StepMode::Into => depth != self.origin_depth || line != self.origin_line,
        }
    }
}

/// The innermost (currently executing) frame's `(source, line)`, or `None` before the first
/// statement. The identity a step is measured against — via the line table, so it is defined at
/// every real instruction (a step lands only where this is `Some`).
fn top_line(view: &DebugView, sources: &SourceMap) -> Option<(u32, u32)> {
    let span = view.frame(view.depth() - 1).line_span()?;
    Some((
        span.source.0,
        sources.source(span.source).line_col(span.start).line,
    ))
}

/// A snapshot of the call stack at a pause: fully owned (no borrow of VM internals), so another
/// thread can serialize it long after `before_op` returned. Innermost (currently executing) frame
/// is first — the order debuggers render stacks in.
#[derive(Debug)]
pub struct PausedState {
    pub frames: Vec<FrameInfo>,
}

/// One captured stack frame: where it is paused (name + source position) and its in-scope locals.
#[derive(Debug)]
pub struct FrameInfo {
    /// The function's debug name (`"main"`, `"Point.mag"`, …).
    pub name: String,
    /// The source file the frame is executing in, if the paused instruction carried a span.
    pub path: Option<String>,
    /// 1-based line/column of the instruction about to execute.
    pub line: u32,
    pub column: u32,
    /// The named locals visible at the pause, in declaration order.
    pub locals: Vec<VarInfo>,
}

/// One local variable's captured name, rendered value, and type.
#[derive(Debug)]
pub struct VarInfo {
    pub name: String,
    pub value: String,
    pub ty: String,
}

/// Whether a local declared at `def_span` is **in scope** at the paused instruction spanning
/// `here` — i.e. its initializing store has already executed.
///
/// A pause always lands at the *start of a source line* (a breakpoint resolves to the line's first
/// pc; a landed step stops when the source line changes), so none of the current line's bindings
/// have run yet: a local is in scope exactly when its declaration is on an *earlier* line. This is
/// deliberately line-granular rather than a raw byte-offset test, which mis-orders a binding whose
/// value expression sits to the right of its name — in `b = f(x)` the name `b` is at an earlier
/// offset than the `f(x)` the pause resolves to, yet `b` is not stored until the line completes, so
/// a byte test would surface `b` (as its pre-store `unit`) while stopped on its own line.
fn local_in_scope(def_span: Span, here: Option<Span>, sources: &SourceMap) -> bool {
    match here {
        // Same source (a frame's locals and its paused span always are): earlier line ⇒ declared.
        Some(h) if def_span.source == h.source => {
            sources
                .source(def_span.source)
                .line_col(def_span.start)
                .line
                < sources.source(h.source).line_col(h.start).line
        }
        // No paused span (a spanless prologue), or the defensive cross-file case: keep the local.
        _ => true,
    }
}

/// Snapshot the live [`DebugView`] into an owned [`PausedState`]. Walks the frame stack innermost
/// first; for each frame records its name, source position, and the locals that are *in scope at
/// the pause* ([`local_in_scope`]) — a local declared later (or by the very line we're stopped on)
/// has a pinned-but-unassigned register, so filtering keeps the yet-to-exist names out of the view.
pub fn capture(view: &DebugView, sources: &SourceMap) -> PausedState {
    let mut frames = Vec::with_capacity(view.depth());
    for i in (0..view.depth()).rev() {
        let frame = view.frame(i);
        let line_span = frame.line_span();
        let (path, line, column) = match line_span {
            Some(span) => {
                let source = sources.source(span.source);
                let lc = source.line_col(span.start);
                (Some(source.name().to_string()), lc.line, lc.col)
            }
            None => (None, 0, 0),
        };
        let mut locals: Vec<VarInfo> = frame
            .locals()
            .filter(|(_, def_span, _)| local_in_scope(*def_span, line_span, sources))
            .map(|(name, _, value)| VarInfo {
                name: name.to_string(),
                value: value.display(),
                // The shared surface-syntax type spelling (`List<int>` — the same the LSP hover and
                // REPL `:type` show), falling back to the coarse kind name for untagged primitives.
                ty: value.type_display(),
            })
            .collect();
        // The top-level (`main`, proto 0) frame's scope also holds the program's top-level value
        // bindings, which are stored in the globals tier rather than this frame's registers (a named
        // function is sealed and cannot see them, so they appear on no other frame). Only the main
        // frame carries them; an isolate worker's bottom frame is its own entry fn, not proto 0.
        if view.proto_at(i) == 0 {
            locals.extend(view.top_level_bindings().map(|(name, value)| VarInfo {
                name: name.to_string(),
                value: value.display(),
                ty: value.type_display(),
            }));
        }
        frames.push(FrameInfo {
            name: frame.name().unwrap_or("<anonymous>").to_string(),
            path,
            line,
            column,
            locals,
        });
    }
    PausedState { frames }
}

/// The in-scope local *names* of snapshot frame `frame` (innermost-first indexing, as debuggers
/// number stack frames) — the parameter list a console fragment is checked against before it runs
/// (session-checker C3; the checking itself lives with the checker, keeping the VM checker-free).
/// Uses the same [`local_in_scope`] rule as [`capture`], so the console sees exactly the names the
/// Variables panel shows — a name declared by the paused line (not yet stored) is not a parameter,
/// and referencing it is an ordinary undefined-name error rather than an `int`-and-`unit` confusion.
pub fn frame_param_names(
    view: &DebugView,
    frame: usize,
    sources: &SourceMap,
) -> Result<Vec<String>, String> {
    let Some(view_idx) = view.depth().checked_sub(frame + 1) else {
        return Err(format!("no frame {frame} in the paused stack"));
    };
    let f = view.frame(view_idx);
    let here = f.line_span();
    Ok(f.locals()
        .filter(|(_, def_span, _)| local_in_scope(*def_span, here, sources))
        .map(|(name, _, _)| name.to_string())
        .collect())
}
