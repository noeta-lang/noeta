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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use noeta_bytecode::Module;
use noeta_span::SourceMap;
use noeta_vm::{DebugAction, Debugger};
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
        events: Sender<Value>,
        resume: Receiver<Resume>,
    ) -> DapDebugger {
        DapDebugger {
            stops,
            stop_on_entry,
            entered: false,
            terminate,
            events,
            resume,
        }
    }

    /// Emit a `stopped` event and block until the adapter resumes or terminates the run.
    fn pause(&mut self, reason: &str) -> DebugAction {
        let _ = self.events.send(event(
            "stopped",
            json!({
                "reason": reason,
                "threadId": MAIN_THREAD_ID,
                "allThreadsStopped": true,
            }),
        ));
        match self.resume.recv() {
            // A spurious continue after a termination request still terminates.
            Ok(Resume::Continue) if !self.terminate.load(Ordering::Relaxed) => {
                DebugAction::Continue
            }
            Ok(Resume::Continue) => DebugAction::Terminate,
            // Terminate, or the adapter dropped the channel (session gone).
            Ok(Resume::Terminate) | Err(_) => DebugAction::Terminate,
        }
    }
}

impl Debugger for DapDebugger {
    fn before_op(&mut self, proto: u32, pc: usize) -> DebugAction {
        if self.terminate.load(Ordering::Relaxed) {
            return DebugAction::Terminate;
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry");
        }
        if self.stops.contains(&(proto, pc)) {
            return self.pause("breakpoint");
        }
        DebugAction::Continue
    }
}
