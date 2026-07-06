//! The [`Debugger`] the VM consults before each instruction, plus the breakpoint resolution that
//! turns editor `(file, line)` requests into the instruction positions it stops at.
//!
//! The VM knows only prototypes and program counters; the editor speaks files and lines. Resolution
//! bridges them once, up front: for every instruction that carries a source span, map it to a
//! `(file, line)` (via the [`SourceMap`]) and, when that line is a requested breakpoint, record the
//! **first** such instruction per prototype as a stop position. At run time [`DapDebugger::before_op`]
//! is then a cheap set membership test; when it hits (or on stop-on-entry) it emits a `stopped` event
//! and blocks the run thread on a resume channel until the adapter says continue or terminate.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use noeta_bytecode::Module;
use noeta_span::SourceMap;
use noeta_vm::{DebugAction, DebugView, Debugger};
use serde_json::{Value, json};

use crate::MAIN_THREAD_ID;
use crate::protocol::event;

/// A resume command sent from the adapter to a paused run.
pub enum Resume {
    /// Leave the pause and keep running.
    Continue,
    /// Abandon the run (the client disconnected).
    Terminate,
}

/// The paused stack, captured by the run worker at a pause and read by the adapter thread to answer
/// `stackTrace` / `scopes` / `variables`. `None` whenever the program is running (before the first
/// pause, or after a resume). A `Mutex` because the two threads touch it at disjoint times — the
/// worker writes it just before blocking and clears it on resume, the adapter reads it only while the
/// worker is blocked — so contention is nil; the lock is purely for safe hand-off.
pub type Paused = Arc<Mutex<Option<PausedState>>>;

/// A snapshot of the call stack at a pause: fully owned (no borrow of VM internals), so the adapter
/// thread can serialize it long after `before_op` returned. Innermost (currently executing) frame is
/// first, matching the order DAP wants stack frames in.
pub struct PausedState {
    pub frames: Vec<FrameInfo>,
}

/// One captured stack frame: where it is paused (name + source position) and its in-scope locals.
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
pub struct VarInfo {
    pub name: String,
    pub value: String,
    pub ty: String,
}

/// Resolve editor breakpoint requests (`path → 1-based lines`) against the compiled program into the
/// set of `(proto, pc)` instruction positions the VM should stop at: the first spanned instruction of
/// each requested line, per prototype. A line with no matching instruction simply yields nothing (the
/// breakpoint is unverifiable — it lands on a blank/comment line or code that compiled away).
pub fn resolve_breakpoints(
    module: &Module,
    sources: &SourceMap,
    requested: &HashMap<String, Vec<u32>>,
) -> HashSet<(u32, usize)> {
    let mut stops = HashSet::new();
    for (proto_idx, chunk) in module.protos.iter().enumerate() {
        // First spanned pc per (file, line) within this prototype — a line's instructions are
        // contiguous, so the lowest pc is where the line's execution begins.
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        for (pc, op) in chunk.code.iter().enumerate() {
            let Some(span) = op.span() else { continue };
            let source = sources.source(span.source);
            let line = source.line_col(span.start).line;
            if !line_requested(requested, source.name(), line) {
                continue;
            }
            if seen.insert((span.source.0, line)) {
                stops.insert((proto_idx as u32, pc));
            }
        }
    }
    stops
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

/// The [`Debugger`] the VM calls before every instruction on the debug run thread. It pauses at a
/// resolved breakpoint (or once at entry), reports the stop, and blocks until the adapter resumes it.
pub struct DapDebugger {
    /// `(proto, pc)` positions a breakpoint resolves to.
    stops: HashSet<(u32, usize)>,
    /// Pause once before the first instruction runs.
    stop_on_entry: bool,
    /// Whether the entry pause has already happened.
    entered: bool,
    /// Set by the adapter to abandon the run even while it is executing (not paused) — checked each op.
    terminate: Arc<AtomicBool>,
    /// The source map, to resolve each frame's instruction span to a file + line while capturing a
    /// pause. Held (a per-run clone) so `capture` needs nothing from the adapter thread.
    sources: SourceMap,
    /// Where a pause publishes the captured stack for the adapter thread to read; cleared on resume.
    paused: Paused,
    /// Outgoing events (a `stopped` when it pauses), funnelled to the writer thread.
    events: Sender<Value>,
    /// Resume commands from the adapter; recv blocks the run thread while paused.
    resume: Receiver<Resume>,
}

impl DapDebugger {
    pub fn new(
        stops: HashSet<(u32, usize)>,
        stop_on_entry: bool,
        terminate: Arc<AtomicBool>,
        sources: SourceMap,
        paused: Paused,
        events: Sender<Value>,
        resume: Receiver<Resume>,
    ) -> DapDebugger {
        DapDebugger {
            stops,
            stop_on_entry,
            entered: false,
            terminate,
            sources,
            paused,
            events,
            resume,
        }
    }

    /// Capture the paused stack, emit a `stopped` event, and block until the adapter resumes or
    /// terminates the run. The snapshot is published *before* the event so a `stackTrace` the client
    /// sends on seeing `stopped` always finds it; it is cleared on resume so a late request after
    /// `continue` reads an empty stack rather than stale frames.
    fn pause(&mut self, reason: &str, view: &DebugView) -> DebugAction {
        *self.paused.lock().unwrap() = Some(capture(view, &self.sources));
        let _ = self.events.send(event(
            "stopped",
            json!({
                "reason": reason,
                "threadId": MAIN_THREAD_ID,
                "allThreadsStopped": true,
            }),
        ));
        let action = match self.resume.recv() {
            // A spurious continue after a termination request still terminates.
            Ok(Resume::Continue) if !self.terminate.load(Ordering::Relaxed) => {
                DebugAction::Continue
            }
            Ok(Resume::Continue) => DebugAction::Terminate,
            // Terminate, or the adapter dropped the channel (session gone).
            Ok(Resume::Terminate) | Err(_) => DebugAction::Terminate,
        };
        *self.paused.lock().unwrap() = None;
        action
    }
}

impl Debugger for DapDebugger {
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
        if self.terminate.load(Ordering::Relaxed) {
            return DebugAction::Terminate;
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry", view);
        }
        if self.stops.contains(&(proto, pc)) {
            return self.pause("breakpoint", view);
        }
        DebugAction::Continue
    }
}

/// Snapshot the live [`DebugView`] into an owned [`PausedState`]. Walks the frame stack innermost
/// first (the order DAP renders it); for each frame records its name, source position, and the locals
/// that are *in scope at the pause* — those whose binding begins before the instruction about to run.
/// A local declared later in the function has a pinned-but-unassigned register, so filtering by
/// declaration span keeps the yet-to-exist names out of the view.
fn capture(view: &DebugView, sources: &SourceMap) -> PausedState {
    let mut frames = Vec::with_capacity(view.depth());
    for i in (0..view.depth()).rev() {
        let frame = view.frame(i);
        let op_span = frame.op_span();
        let (path, line, column) = match op_span {
            Some(span) => {
                let source = sources.source(span.source);
                let lc = source.line_col(span.start);
                (Some(source.name().to_string()), lc.line, lc.col)
            }
            None => (None, 0, 0),
        };
        let locals = frame
            .locals()
            .filter(|(_, def_span, _)| match op_span {
                // In scope iff its binding begins strictly before the paused instruction. Strict, so a
                // local being introduced by the very instruction we're stopped before isn't shown as
                // bound yet.
                Some(here) => def_span.start < here.start,
                // No paused span to compare against: show every named local rather than hide them all.
                None => true,
            })
            .map(|(name, _, value)| VarInfo {
                name: name.to_string(),
                value: value.display(),
                ty: value.type_name().to_string(),
            })
            .collect();
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
