//! The profile session's compile + run path: load → check → compile (tier-0, JIT-off) → execute.
//!
//! This is the *same* production pipeline `noeta run` drives — the shared front half
//! (`noeta_runner::compile::load_default_project`: dependency packages, tier machinery, per-source
//! editions) then check → compile → VM — with one profiler-specific choice: the program runs
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
use noeta_diagnostics::render_mapped;
use noeta_span::SourceMap;
use noeta_vm::{ProfileHook, VmBackend};

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
    // The shared front half (drift firewall): the profiler sees the same dependency packages and
    // editions `noeta run` resolves — a program that runs must also be profilable.
    let loaded = match noeta_runner::compile::load_default_project(path) {
        Ok(loaded) => loaded,
        Err(failure) => {
            let (text, code) = failure.to_text();
            return Err(RunOutput::failed(text, i32::from(code)));
        }
    };

    let checked = loaded.check();
    if !checked.diagnostics.is_empty() {
        return Err(RunOutput::failed(
            render_mapped(&loaded.sources, checked.diagnostics.iter()),
            1,
        ));
    }

    match noeta_compiler::compile_with_sites(
        &loaded.program,
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
            sources: loaded.sources,
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
/// collecting the program's output. With `hook = Some(..)` a [`ProfileHook`] is consulted before
/// every op (the instrumenting/sampling collector) and handed back for its results to be reclaimed;
/// with `None` it is the plain tier-0 run (P0). Never panics on ordinary failure: a host/executor
/// that cannot start becomes a `stderr` chunk with a non-zero exit.
pub fn run(
    compiled: &Compiled,
    hook: Option<Box<dyn ProfileHook>>,
) -> (RunOutput, Option<Box<dyn ProfileHook>>) {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_runtime::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => {
            return (
                RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
                hook,
            );
        }
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_runtime::RealExecutor::new() {
        Ok(executor) => Box::new(executor),
        Err(err) => {
            return (
                RunOutput::failed(format!("noeta: cannot start executor: {err}\n"), 2),
                hook,
            );
        }
    };

    // We wrap only the run itself in the wall-clock measurement, not the compile — a profile reports
    // where the *program* spends time, not the toolchain. Both paths pin tier-0 (JIT never armed).
    let backend = VmBackend::new();
    let start = Instant::now();
    let (result, hook, trace) = match hook {
        // Instrumenting/sampling run: the hook rides the per-op seam and comes back with its results.
        Some(hook) => {
            let (result, hook, trace) =
                backend.run_module_profiled(&compiled.module, host, executor, hook);
            (result, Some(hook), trace)
        }
        // Plain tier-0 run (`run_module_debug(.., None)` is the JIT-off path; no per-op consult).
        None => {
            let (result, trace) = backend.run_module_debug(&compiled.module, host, executor, None);
            (result, None, trace)
        }
    };
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
    (
        RunOutput {
            chunks,
            exit_code: result.exit_code,
            wall,
        },
        hook,
    )
}
