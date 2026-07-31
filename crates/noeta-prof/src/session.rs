//! The profile session's compile + run path: load → check → compile → execute.
//!
//! This is the *same* production pipeline `noeta run` drives — the shared front half
//! (`noeta_runner::compile::load_default_project`: dependency packages, tier machinery, per-source
//! editions) then check → compile → VM. Tiering is the profiler's one axis: by default the program
//! runs with the **JIT unarmed** (tier-0), so every frame is interpreter-executed and there is a real
//! pc + an op boundary at each step for a collector to observe — the debugger's choice too. The
//! tier-1 sampling mode (`noeta profile --jit`) instead arms the production hot-counter JIT
//! ([`noeta_vm::Tiering::Hot`]); hot prototypes run native and the sampler attributes their wall time
//! at the JIT trampoline (function-level, tier-1-labeled). Unlike `noeta dap` it compiles **without**
//! debug info: the profiler only needs function names + source lines, and those live in the
//! always-emitted line tables on every `Chunk` (`name` / `def_span` / `line_table`), not the
//! debug-gated locals table.
//!
//! Compilation is split from execution so a later slice can attach a collector to the compiled
//! [`Module`] before the run starts (and resolve `proto → name @ file:line` against its chunks).

use std::path::Path;
use std::sync::Arc;
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
    /// The compile's non-blocking diagnostics, already rendered. A warning does not stop a profile
    /// run — the program is well-formed and profiling it is exactly what was asked — so it rides
    /// here and is replayed as the run's first `stderr` chunk.
    pub warnings: String,
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
    /// Prototypes the tier-1 JIT compiled during the run (the `--jit-stats`-style promotion count),
    /// `Some` only on a tier-1 (`jit`-armed) sampling run built with the `jit` feature. `None` on
    /// every tier-0 run (and on a `jit`-armed run of a build without the feature — nothing compiled).
    pub jit_compiled: Option<usize>,
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
            jit_compiled: None,
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
    // Errors only — a warning is advisory and must not cost you the profile.
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return Err(RunOutput::failed(
            render_mapped(&loaded.sources, checked.diagnostics.iter()),
            1,
        ));
    }
    let warnings = render_mapped(&loaded.sources, checked.diagnostics.iter());

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
            warnings,
        }),
        Err(u) => Err(RunOutput::failed(
            match u.diagnostic() {
                Some(diagnostic) => {
                    noeta_diagnostics::render_mapped(&loaded.sources, std::iter::once(&diagnostic))
                }
                None => format!("noeta: {u}\n"),
            },
            1,
        )),
    }
}

/// Run an already-compiled program on the real host, timing the run and collecting the program's
/// output. With `hook = Some(..)` a [`ProfileHook`] is consulted before every op (the
/// instrumenting/sampling collector) and handed back for its results to be reclaimed; with `None` it
/// is the plain run (P0). `tier1` arms the production hot-counter JIT (tier-1 sampling — the sampler
/// attributes native time at the JIT trampoline); off, the run is pinned tier-0 (JIT unarmed) so
/// every frame is interpreter-executed and observable at an op boundary, as the debugger/profiler
/// have always required. Never panics on ordinary failure: a host/executor that cannot start becomes
/// a `stderr` chunk with a non-zero exit.
pub fn run(
    compiled: &Compiled,
    hook: Option<Box<dyn ProfileHook>>,
    isolate_profiler: Option<(noeta_vm::ProfileHookFactory, noeta_vm::ProfileSink)>,
    tier1: bool,
) -> (RunOutput, Option<Box<dyn ProfileHook>>) {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_host_real::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => {
            return (
                RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
                hook,
            );
        }
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
        Ok(executor) => Box::new(executor),
        Err(err) => {
            return (
                RunOutput::failed(format!("noeta: cannot start executor: {err}\n"), 2),
                hook,
            );
        }
    };

    // We wrap only the run itself in the wall-clock measurement, not the compile — a profile reports
    // where the *program* spends time, not the toolchain. Tier-0 pinned (default Tiering::Off; the
    // JIT is never armed). Real isolates ARE armed — `noeta run` parity, so a parallel program
    // profiles as it actually executes — with the per-isolate profile seam threaded through when
    // the caller supplies one.
    let isolate_factory: noeta_vm::IsolateFactory = Arc::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_host_real::RealHost::new().expect("cannot start an isolate's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_host_real::RealExecutor::new().expect("cannot start an isolate's async executor"),
        );
        (host, executor)
    });
    // Tier-1 sampling arms the production hot-counter tier (`Tiering::Hot`, off-thread compile —
    // exactly what `noeta run` uses), so the profile reflects what actually ships; the sampler banks
    // native segments at the JIT trampoline. Off, the run stays `Tiering::Off` (tier-0 pinned). The
    // `Tiering` enum is always available; without the `jit` feature `Hot` is a no-op (everything
    // interprets), so a `jit`-armed run of a feature-less build stays observably tier-0.
    let tiering = if tier1 {
        noeta_vm::Tiering::Hot
    } else {
        noeta_vm::Tiering::Off
    };
    let backend = VmBackend::new();
    let start = Instant::now();
    let outcome = backend.run_module_with(
        &compiled.module,
        noeta_vm::RunOptions {
            host,
            executor,
            profiler: hook,
            isolates: Some((Arc::new(compiled.module.clone()), isolate_factory)),
            isolate_profiler,
            tiering,
            // Compile the outstanding queue at exit so the promotion count reported below is final
            // rather than racing the program's own runtime. Off-path when `jit` is disabled.
            #[cfg(feature = "jit")]
            drain_at_exit: tier1,
            ..noeta_vm::RunOptions::default()
        },
    );
    // The tier-1 promotion count (`--jit-stats`-style): how many prototypes went native. Only
    // meaningful on a `jit`-feature build's armed run; `None` otherwise.
    #[cfg(feature = "jit")]
    let jit_compiled = tier1.then_some(outcome.stats.compiled);
    #[cfg(not(feature = "jit"))]
    let jit_compiled: Option<usize> = None;
    let (result, hook, trace) = (outcome.result, outcome.profiler, outcome.trace);
    let wall = start.elapsed();

    let mut chunks = Vec::new();
    // Compile-time facts about the file come before the file's own output.
    if !compiled.warnings.is_empty() {
        chunks.push(OutputChunk {
            category: "stderr",
            text: compiled.warnings.clone(),
        });
    }
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
            jit_compiled,
        },
        hook,
    )
}
