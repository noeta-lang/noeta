//! `noeta run` — execute a program file (source or `.noeb` bundle) on the real host,
//! plus the shared check-then-execute helpers the other verbs ride.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_pm::manifest;
use noeta_runner::compile::Loaded;
use noeta_runner::compile::compile_whole_file_with;
use noeta_runner::compile_real;
use noeta_span::SourceMap;

use crate::compose;
use crate::output::emit_diagnostics_mapped;

/// Type-check and run a loaded program, writing stdout to the real stdout and rendering any
/// diagnostics to stderr — each against the source its span belongs to (via the `SourceMap`).
/// Returns the process exit code. `loaded.program` is the linked program, possibly after
/// dev-tier activation or an entry-call synthesis (extension commands).
pub(crate) fn run_program(loaded: &Loaded, args: Vec<String>) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `Loaded::check` (editions threaded structurally) produces both the gate diagnostics and
    // the `type_of` site map the backend needs, so the checker runs exactly once (it previously
    // ran again inside the backend).
    let checked = loaded.check();
    // Report everything the front half found — activation's and the check's — *then* decide. Only an
    // **error** stops the program: a warning describes well-formed code, and a lint that refuses to
    // run what `noeta check` calls fine is a hard stop wearing a nudge's clothes. Diagnostics go out
    // first, before a single byte of the program's own output, because they are compile-time facts
    // about the whole file; interleaving them with the run would misattribute them to whatever line
    // happened to be printing.
    emit_diagnostics_mapped(
        &loaded.sources,
        loaded.warnings.iter().chain(checked.diagnostics.iter()),
    );
    if noeta_diagnostics::has_errors(&checked.diagnostics) {
        return 1;
    }

    match execute_real_host(&loaded.program, &checked, args, true, None) {
        // The shared run epilogue (audit row 1): program stdout, program stderr, diagnostics,
        // traceback — one rendering, used by every execution surface. This returns the program's
        // *unclamped* exit code (the caller converts), not the process status.
        Ok((result, trace)) => {
            noeta_backend::RunTail::render_colored(
                &result,
                &trace,
                &loaded.sources,
                noeta_diagnostics::stderr_color(),
            )
            .emit_status();
            result.exit_code
        }
        // An internal compile failure renders like any other diagnostic when the compiler knew
        // where it stopped — the source map is right here.
        Err(u) => {
            let code =
                noeta_runner::CompileFailure::from_unsupported(&loaded.sources, &u).report_u8();
            let _ = io::stderr().flush();
            i32::from(code)
        }
    }
}

/// Execute an already-checked program against the **real host** (real `env`/`args`, real-disk IO)
/// on a per-isolate tokio runtime (M2.3), returning its [`RunResult`].
///
/// Real execution runs on the **VM** backend (isolates I.4a). The tree-walker's `Rc`-based values are
/// `!Send`, so it can never carry real inter-isolate parallelism (isolates I.4); the VM's NaN-boxed,
/// thread-local heap is the one the shared-region borrow-sharing (I.3) and the coming `RealScheduler`
/// (I.4b) build on. The conformance differential still runs *both* backends over the deterministic
/// sandbox, so this real-host path is never compared backend-to-backend. Shared by `noeta run` and the
/// `@test` runner so both execute a program identically.
/// `live_output` streams the program's output to the terminal as it is produced rather than
/// batch-capturing it into the returned [`noeta_backend::RunResult`] — `true` for a foreground
/// `noeta run`, `false` for the `@test` runner, whose report *is* the captured stdout.
/// `cancel` arms the run's cooperative stop request (`noeta_vm::RunOptions::cancel`): `None` for an
/// ordinary run, `Some` for a bounded `@test` case the runner may need to ask to stop.
pub(crate) fn execute_real_host(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
    args: Vec<String>,
    live_output: bool,
    cancel: Option<noeta_vm::CancelFlag>,
) -> Result<(noeta_backend::RunResult, Vec<noeta_vm::TraceFrame>), noeta_compiler::Unsupported> {
    let (result, trace, _) = run_module_real_host(
        std::sync::Arc::new(compile_real(program, checked)?),
        args,
        false,
        live_output,
        cancel,
    );
    Ok((result, trace))
}

/// Run an already-compiled [`Module`] against the real host — the shared execution core of
/// [`execute_real_host`] (source path) and the `.noeb` bundle runner (P-AOT L1.2), which loads a
/// module directly with no source to compile.
/// The p2p application namespace for the running program (p2p P3.4): the package name of the
/// nearest `noeta.toml` (stable and unique per project) when in a package, else the entry script's
/// file stem — so two different Noeta apps never share one p2p identity/store dir. `None` ⇒ let the
/// runtime fall back to its own default (the executable's file stem). `args[0]` is the entry path.
pub(crate) fn p2p_app_namespace(args: &[String]) -> Option<String> {
    if let Ok(cwd) = std::env::current_dir()
        && let Some(path) = manifest::find(&cwd)
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(parsed) = manifest::Manifest::parse(&text)
        && let Some(pkg) = parsed.package()
    {
        return Some(format!("{}/{}", pkg.name.company, pkg.name.package));
    }
    args.first()
        .map(std::path::Path::new)
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
}

/// The p2p application namespace for a source run: the CLI derives it from the nearest `noeta.toml`
/// (via [`p2p_app_namespace`]) and passes it into the shared execution core (`noeta-runner`), which
/// is package-manager-free by design. A convenience so the CLI's callers read the same as before the
/// core was extracted.
pub(crate) fn run_module_real_host(
    module: std::sync::Arc<noeta_bytecode::Module>,
    args: Vec<String>,
    jit_report: bool,
    live_output: bool,
    cancel: Option<noeta_vm::CancelFlag>,
) -> (
    noeta_backend::RunResult,
    Vec<noeta_vm::TraceFrame>,
    Option<noeta_vm::JitReport>,
) {
    let app_id = p2p_app_namespace(&args);
    noeta_runner::run_module_real_host(module, args, app_id, jit_report, live_output, cancel)
}

/// P-AOT L2: detect and run a bundle stapled onto this executable (a `noeta build --exe` artifact),
/// returning its exit code — or `None` when this is the plain toolchain binary (no trailer), so the
/// normal CLI runs. Delegates to the shared runner core, supplying the CLI's `noeta.toml`-aware p2p
/// namespace resolver (the lean `noeta-runner` binary uses the file-stem default instead).
pub(crate) fn try_run_stapled() -> Option<ExitCode> {
    noeta_runner::try_run_stapled(p2p_app_namespace)
}

/// The argument vector a `noeta run` presents to the program via `args.all()`: the entry path as the
/// program name (argv[0]) followed by any pass-through args given after `--`. This mirrors a shipped
/// `noeta build --exe` binary, whose `args.all()` is the real process argv (`[<binary>, <args…>]`)
/// when invoked directly — so a program observes the identical argv from source or as an executable.
pub(crate) fn program_args(entry: &std::path::Path, passthrough: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(passthrough.len() + 1);
    args.push(entry.display().to_string());
    args.extend(passthrough.iter().cloned());
    args
}

pub(crate) fn cmd_run(
    file: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
    jit_stats: bool,
    args: &[String],
) -> ExitCode {
    // The compose probe hands back the graph it resolved (default selection) so the compile
    // below doesn't resolve it again (audit-5 F2).
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
    // P-AOT L1.2: a `.noeb` bundle runs directly — no source, no compile. Sniff the magic (cheap,
    // and we need the bytes to load it anyway); anything else is source, handled below. Tiers are a
    // *build*-time concern (they are already baked into the bundle), so `--tier`/`--target` on a
    // bundle run are meaningless — reject them rather than silently ignore.
    if let Ok(bytes) = std::fs::read(file)
        && noeta_bundle::is_bundle(&bytes)
    {
        if !tiers.is_empty() || target.is_some() {
            eprintln!(
                "noeta: --tier/--target apply at build time; a .noeb bundle is already built"
            );
            return ExitCode::from(2);
        }
        return cmd_run_bundle(file, &bytes, program_args(file, args), jit_stats);
    }

    // Everything else — resolve tiers, consult the startup cache, and (on a miss) load → check →
    // compile — is the shared whole-file pipeline. On success run the module with the program's
    // pass-through args; on failure report it.
    match compile_whole_file_with(
        file,
        tiers,
        target,
        no_cache,
        resolved.map(|g| noeta_runner::compile::ResolvedFront {
            packages: g.packages,
            package_uses: g.package_uses,
        }),
    ) {
        // Warnings first, then the program: a compile-time fact about the file belongs before the
        // file's own output, not spliced into it.
        Ok(compiled) => {
            emit_diagnostics_mapped(&compiled.sources, compiled.warnings.iter());
            run_compiled_module(
                compiled.module,
                &compiled.sources,
                program_args(file, args),
                jit_stats,
            )
        }
        Err(failure) => failure.report(),
    }
}

/// Run a `.noeb` bundle (P-AOT L1.2): decode the module from the versioned container and execute it
/// on the real host, exactly as a source run does after compiling — but with no source to compile
/// or type-check (both happened at build time). A runtime abort's diagnostics/trace carry spans but
/// the bundle ships no source text, so they render against a synthetic empty source (message + code
/// + location show; no code snippet) — the honest cost of a source-free artifact.
pub(crate) fn cmd_run_bundle(
    file: &std::path::Path,
    bytes: &[u8],
    args: Vec<String>,
    jit_stats: bool,
) -> ExitCode {
    let app_id = p2p_app_namespace(&args);
    noeta_runner::run_bundle_bytes(file, bytes, args, app_id, jit_stats)
}

/// Run a compiled module and render its output/diagnostics/trace/JIT-report, returning the exit
/// code. Delegates to the shared execution core (`noeta-runner`) after deriving the p2p app
/// namespace from the workspace — the CLI's package-manager-aware wrapper over the lean runner.
pub(crate) fn run_compiled_module(
    module: std::sync::Arc<noeta_bytecode::Module>,
    sources: &SourceMap,
    args: Vec<String>,
    jit_stats: bool,
) -> ExitCode {
    let app_id = p2p_app_namespace(&args);
    noeta_runner::run_compiled_module(module, sources, args, app_id, jit_stats)
}
