//! The debug session's compile + run path: load → check → compile (debug, JIT-off) → execute.
//!
//! This is the *same* production pipeline `noeta run` drives (`noeta_loader::load` →
//! `noeta_check::check_all` → compile → VM), with two debug-specific choices: the program is compiled
//! **with debug info** (`compile_with_sites(..., debug = true)` — reg→name locals, proto names/spans)
//! and run with the **JIT unarmed** and a [`Debugger`] attached, so every frame stays tier-0 and
//! inspectable. Its stdout is captured (already the VM's behavior) and returned rather than printed —
//! the adapter forwards it as `output` events, keeping the real stdout free for the DAP wire.
//!
//! Compilation is split from execution so the adapter can resolve breakpoints against the compiled
//! [`Module`] + [`SourceMap`] before the run starts.

use std::path::Path;

use noeta_bytecode::Module;
use noeta_diagnostics::{render, render_mapped};
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
    let linked = match noeta_loader::load(path, noeta_pm::manifest::root_edition(path)) {
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

    // The session flavor keeps the checker alive (C3): console fragments will check against the
    // typing environment this whole-program check accumulates.
    let (checked, checker) =
        noeta_check::check_all_session_with(&linked.program, linked.editions.clone());
    if !checked.diagnostics.is_empty() {
        return Err(RunOutput::failed(
            render_mapped(&linked.sources, checked.diagnostics.iter()),
            1,
        ));
    }

    match compile_checked(&linked.program, &checked) {
        Ok((module, session)) => Ok(Compiled {
            module,
            sources: linked.sources,
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
    let host: Box<dyn noeta_stdlib::Host> = match noeta_runtime::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_runtime::RealExecutor::new() {
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
    // there is a call chain" rule as `noeta run`).
    if trace.len() >= 2 {
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
