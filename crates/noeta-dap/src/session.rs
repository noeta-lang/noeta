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
use noeta_diagnostics::{Diagnostic, render};
use noeta_span::SourceMap;
use noeta_vm::{Debugger, VmBackend};

/// A program compiled and ready to run under the debugger: the bytecode plus the source map that
/// resolves each instruction's span back to a file + line.
pub struct Compiled {
    pub module: Module,
    pub sources: SourceMap,
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
            render_all(&linked.sources, checked.diagnostics.iter()),
            1,
        ));
    }

    match compile_checked(&linked.program, &checked) {
        Ok(module) => Ok(Compiled {
            module,
            sources: linked.sources,
        }),
        Err(reason) => Err(RunOutput::failed(format!("noeta: {reason}\n"), 1)),
    }
}

/// Run an already-compiled program (JIT unarmed) with an optional [`Debugger`] attached, collecting
/// its output. Never panics on ordinary failure: a host/executor that cannot start becomes a `stderr`
/// chunk with a non-zero exit.
pub fn run_compiled(compiled: &Compiled, debugger: Option<Box<dyn Debugger>>) -> RunOutput {
    let host: Box<dyn noeta_stdlib::Host> = match noeta_runtime::RealHost::new() {
        Ok(host) => Box::new(host),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start host: {err}\n"), 2),
    };
    let executor: Box<dyn noeta_stdlib::Executor> = match noeta_runtime::RealExecutor::new() {
        Ok(executor) => Box::new(executor),
        Err(err) => return RunOutput::failed(format!("noeta: cannot start executor: {err}\n"), 2),
    };

    let result = VmBackend::new().run_module_debug(&compiled.module, host, executor, debugger);

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
            text: render_all(&compiled.sources, result.diagnostics.iter()),
        });
    }
    RunOutput {
        chunks,
        exit_code: result.exit_code,
    }
}

/// Compile an already-checked program to a bytecode [`Module`] **with debug info**. Mirrors the CLI's
/// `compile_real` (unpacking `checked`'s site maps into `compile_with_sites`, the checker output
/// contract the compiler consumes without depending on `noeta-check`) but passes `debug = true`.
/// Adding a site map changes that signature, so both call sites fail to compile together — the
/// duplication cannot silently drift.
fn compile_checked(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<Module, String> {
    noeta_compiler::compile_with_sites(
        program,
        checked.type_of_sites.clone(),
        checked.packed_list_sites.clone(),
        checked.map_packed_sites.clone(),
        checked.index_field_sites.clone(),
        checked.ext_call_sites.clone(),
        checked.for_stream_sites.clone(),
        checked.width_sites.clone(),
        checked.f32_literal_sites.clone(),
        checked.construction_sites.clone(),
        &checked.destructor_relevance,
        // Real execution lowers `isolate f(args)` to real OS-thread spawns, as `noeta run` does.
        true,
        // Debug compile: emit the debug-info side-tables (reg->name locals, proto names + spans) and
        // pin named locals through coalescing. This is the one difference from `noeta run`'s compile.
        true,
    )
    .map_err(|u| {
        format!(
            "internal error: the VM cannot compile this program: {}",
            u.reason
        )
    })
}

/// Render each diagnostic against the source its span belongs to (via the [`SourceMap`]), matching
/// the CLI's cross-module diagnostic rendering.
fn render_all<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) -> String {
    let mut text = String::new();
    for diagnostic in diagnostics {
        text.push_str(&render(sources.source(diagnostic.span.source), diagnostic));
    }
    text
}
