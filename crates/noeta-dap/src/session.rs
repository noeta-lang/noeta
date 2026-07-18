//! The debug session's compile + run path: load → check → compile (debug, JIT-off) → execute.
//!
//! This is the *same* production pipeline `noeta run` drives — the shared front half
//! (`noeta_runner::compile::load_default_project`: dependency packages, tier machinery, per-source
//! editions) then check → compile → VM — with two debug-specific choices: the program is compiled
//! **with debug info** (`compile_with_sites(..., debug = true)` — reg→name locals, proto names/spans)
//! and run with the **JIT unarmed** and a [`Debugger`] attached, so every frame stays tier-0 and
//! inspectable. Its stdout is captured (already the VM's behavior) and returned rather than printed —
//! the adapter forwards it as `output` events, keeping the real stdout free for the DAP wire.
//!
//! Compilation is split from execution so the adapter can resolve breakpoints against the compiled
//! [`Module`] + [`SourceMap`] before the run starts.

use std::path::Path;

use noeta_bytecode::Module;
use noeta_diagnostics::render_mapped;
use noeta_span::SourceMap;
use noeta_vm::{Debugger, VmBackend};

/// A program compiled and ready to run under the debugger: the bytecode, the source map that
/// resolves each instruction's span back to a file + line, and the **live session compiler** the
/// checked compile left behind (tooling-unification T3/T5) — console fragments extend it at run
/// time, appending onto the program's own id-spaces.
pub struct Compiled {
    pub module: Module,
    pub sources: SourceMap,
    pub session: noeta_compiler::SessionCompiler,
    /// The session type-checker the checked launch compile left behind (session-checker C3) —
    /// console fragments check against it before running.
    pub checker: noeta_check::SessionChecker,
}

/// One chunk of program output, tagged with the DAP `output`-event category it belongs to.
pub struct OutputChunk {
    /// `"stdout"` for program output, `"stderr"` for diagnostics / loader errors.
    pub category: &'static str,
    pub text: String,
}

/// Everything a run produced: the ordered output chunks to replay as `output` events and the exit
/// code to report in the `exited` event.
pub struct RunOutput {
    pub chunks: Vec<OutputChunk>,
    pub exit_code: i32,
}

impl RunOutput {
    /// A run that never started (unreadable file, parse/check/compile error): the failure as a
    /// `stderr` chunk plus a non-zero code.
    pub fn failed(text: String, exit_code: i32) -> RunOutput {
        RunOutput {
            chunks: vec![OutputChunk {
                category: "stderr",
                text,
            }],
            exit_code,
        }
    }
}

/// Load, type-check, and compile (in debug mode) the program at `path`. On any ordinary failure —
/// unreadable file, parse/check diagnostics, or a compile error — returns the failure already shaped
/// as a [`RunOutput`] (a `stderr` chunk + non-zero exit) for the adapter to replay.
pub fn compile_file(path: &Path) -> Result<Compiled, RunOutput> {
    // The shared front half (drift firewall): the debugger sees the same dependency packages and
    // editions `noeta run` resolves — a program that runs must also be debuggable.
    let loaded = match noeta_runner::compile::load_default_project(path) {
        Ok(loaded) => loaded,
        Err(failure) => {
            let (text, code) = failure.to_text();
            return Err(RunOutput::failed(text, i32::from(code)));
        }
    };

    // The session flavor keeps the checker alive (C3): console fragments will check against the
    // typing environment this whole-program check accumulates.
    let (checked, checker) = loaded.check_session();
    if !checked.diagnostics.is_empty() {
        return Err(RunOutput::failed(
            render_mapped(&loaded.sources, checked.diagnostics.iter()),
            1,
        ));
    }

    match compile_checked(&loaded.program, &checked) {
        Ok((module, session)) => Ok(Compiled {
            module,
            sources: loaded.sources,
            session,
            checker,
        }),
        Err(reason) => Err(RunOutput::failed(format!("noeta: {reason}\n"), 1)),
    }
}

/// Run an already-compiled program (JIT unarmed) with an optional [`Debugger`] attached, collecting
/// its output. Consumes the [`Compiled`]'s session — the run owns it from here (console fragments
/// extend it on the run worker). Never panics on ordinary failure: a host/executor that cannot
/// start becomes a `stderr` chunk with a non-zero exit.
pub fn run_compiled(compiled: Compiled, debugger: Option<Box<dyn Debugger>>) -> RunOutput {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_host_real::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_host_real::RealExecutor::new() {
        Ok(executor) => Box::new(executor),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start executor: {err}\n"), 2),
    };

    let (result, trace) = VmBackend::new().run_module_debug_session(
        &compiled.module,
        compiled.session,
        host,
        executor,
        debugger,
    );

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
    // The abort's stack trace, after the diagnostic it belongs to (same rendering + same "only when
    // there is a call chain" rule as `noeta run`). A **user-requested stop** (the debug UI's stop
    // button → `DebugAction::Terminate`) unwinds with NO diagnostic recorded — an orphan trace
    // there reads as a crash, so a trace only ever accompanies its diagnostic.
    if !result.diagnostics.is_empty() && trace.len() >= 2 {
        chunks.push(OutputChunk {
            category: "stderr",
            text: noeta_vm::render_trace(&trace, &compiled.sources),
        });
    }
    RunOutput {
        chunks,
        exit_code: result.exit_code,
    }
}

/// Compile an already-checked program to a bytecode [`Module`] **with debug info**, keeping the
/// compiler alive as a session (tooling-unification T3/T5). The same checked compile the CLI's
/// `compile_real` performs (the checker's [`noeta_check::Sites`] bundle threaded through), with two
/// debug-run differences: `debug = true` (the debug-info side-tables — reg→name locals, proto
/// names + spans — and named locals pinned through coalescing), and the session-flavored entry so
/// console fragments can extend the program's id-spaces at run time.
fn compile_checked(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<(Module, noeta_compiler::SessionCompiler), String> {
    noeta_compiler::compile_with_sites_session(
        program,
        checked.sites.clone(),
        // Real execution lowers `isolate f(args)` to real OS-thread spawns, as `noeta run` does.
        true,
        true,
    )
    .map_err(|u| {
        format!(
            "internal error: the VM cannot compile this program: {}",
            u.reason
        )
    })
}
