//! The profile session's compile + run path: load → check → compile (tier-0, JIT-off) → execute.
//!
//! This is the *same* production pipeline `noeta run` drives (`noeta_loader::load` →
//! `noeta_check::check_all` → compile → VM), with one profiler-specific choice: the program runs
//! with the **JIT unarmed** ([`VmBackend::run_module_debug`] with no debugger — the tier-0 run path),
//! so every frame is interpreter-executed and there is a real pc + an op boundary at each step for a
//! later collector to observe. Unlike `noeta dap` it compiles **without** debug info: the profiler
//! only needs function names + source lines, and those live in the always-emitted line tables on
//! every `Chunk` (`name` / `def_span` / `line_table`), not the debug-gated locals table.
//!
//! Compilation is split from execution so a later slice can attach a collector to the compiled
//! [`Module`] before the run starts (and resolve `proto → name @ file:line` against its chunks).

use std::path::Path;
use std::time::{Duration, Instant};

use noeta_bytecode::Module;
use noeta_diagnostics::{render, render_mapped};
use noeta_span::SourceMap;
use noeta_vm::VmBackend;

/// A program compiled tier-0 and ready to profile: the bytecode and the source map that resolves
/// each instruction's span back to a file + line (for `proto → name @ file:line` attribution).
pub struct Compiled {
    pub module: Module,
    pub sources: SourceMap,
}

/// One chunk of program output, tagged with the stream it belongs to. The program's own stdout is
/// replayed verbatim; diagnostics + the abort trace go to stderr — the profiler's *own* report is
/// emitted separately (see [`crate::run`]) so it never pollutes the program's output.
pub struct OutputChunk {
    /// `"stdout"` for program output, `"stderr"` for diagnostics / loader errors.
    pub category: &'static str,
    pub text: String,
}

/// Everything a profiled run produced: the program's output chunks, its exit code, and the
/// wall-clock time the run took (the first, coarsest profiling signal — P0).
pub struct RunOutput {
    pub chunks: Vec<OutputChunk>,
    pub exit_code: i32,
    pub wall: Duration,
}

impl RunOutput {
    /// A run that never started (unreadable file, parse/check/compile error): the failure as a
    /// `stderr` chunk plus a non-zero code and a zero duration.
    pub fn failed(text: String, exit_code: i32) -> RunOutput {
        RunOutput {
            chunks: vec![OutputChunk {
                category: "stderr",
                text,
            }],
            exit_code,
            wall: Duration::ZERO,
        }
    }
}

/// Load, type-check, and compile (tier-0, no debug info) the program at `path`. On any ordinary
/// failure — unreadable file, parse/check diagnostics, or a compile error — returns the failure
/// already shaped as a [`RunOutput`] (a `stderr` chunk + non-zero exit) for the caller to replay.
pub fn compile_file(path: &Path) -> Result<Compiled, RunOutput> {
    let linked = match noeta_loader::load(path) {
        Err(err) => {
            return Err(RunOutput::failed(
                format!("noeta: cannot read {}: {err}\n", path.display()),
                2,
            ));
        }
        Ok(Err(load_diagnostics)) => {
            let mut text = String::new();
            for ld in &load_diagnostics {
                text.push_str(&render(&ld.source, &ld.diagnostic));
            }
            return Err(RunOutput::failed(text, 1));
        }
        Ok(Ok(linked)) => linked,
    };

    let checked = noeta_check::check_all(&linked.program);
    if !checked.diagnostics.is_empty() {
        return Err(RunOutput::failed(
            render_mapped(&linked.sources, checked.diagnostics.iter()),
            1,
        ));
    }

    match noeta_compiler::compile_with_sites(
        &linked.program,
        checked.sites.clone(),
        // Real execution lowers `isolate f(args)` to real OS-thread spawns, as `noeta run` does; the
        // tier-0 run path with no isolate factory falls back to cooperative in-thread tasks, which is
        // exactly what the profiler wants (one thread to sample).
        true,
        // No debug info: the profiler reads the always-emitted line tables, not debug-gated locals.
        false,
    ) {
        Ok(module) => Ok(Compiled {
            module,
            sources: linked.sources,
        }),
        Err(u) => Err(RunOutput::failed(
            format!(
                "noeta: internal error: the VM cannot compile this program: {}\n",
                u.reason
            ),
            1,
        )),
    }
}

/// Run an already-compiled program **tier-0** (JIT unarmed) on the real host, timing the run and
/// collecting the program's output. Never panics on ordinary failure: a host/executor that cannot
/// start becomes a `stderr` chunk with a non-zero exit.
pub fn run_compiled(compiled: Compiled) -> RunOutput {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_runtime::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_runtime::RealExecutor::new() {
        Ok(executor) => Box::new(executor),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start executor: {err}\n"), 2),
    };

    // `run_module_debug(.., None)` is the tier-0 run path (JIT never armed); the `None` debugger
    // means no per-op consult. We wrap only the run itself in the wall-clock measurement, not the
    // compile — a profile reports where the *program* spends time, not the toolchain.
    let start = Instant::now();
    let (result, trace) = VmBackend::new().run_module_debug(&compiled.module, host, executor, None);
    let wall = start.elapsed();

    let mut chunks = Vec::new();
    if !result.stdout.is_empty() {
        chunks.push(OutputChunk {
            category: "stdout",
            text: result.stdout,
        });
    }
    if !result.diagnostics.is_empty() {
        chunks.push(OutputChunk {
            category: "stderr",
            text: render_mapped(&compiled.sources, result.diagnostics.iter()),
        });
    }
    if trace.len() >= 2 {
        chunks.push(OutputChunk {
            category: "stderr",
            text: noeta_vm::render_trace(&trace, &compiled.sources),
        });
    }
    RunOutput {
        chunks,
        exit_code: result.exit_code,
        wall,
    }
}
