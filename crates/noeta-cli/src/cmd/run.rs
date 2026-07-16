//! `noeta run` — execute a program file (source or `.noeb` bundle) on the real host,
//! plus the shared check-then-execute helpers the other verbs ride.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_pm::manifest;
use noeta_runner::{compile_real, compile_whole_file};
use noeta_span::SourceMap;

use crate::compose;
use crate::output::emit_diagnostics_mapped;

/// Type-check and run a program, writing stdout to the real stdout and rendering any diagnostics to
/// stderr — each against the source its span belongs to (via the `SourceMap`). Returns the process
/// exit code. `program` is the loaded program, possibly after dev-tier activation (`cmd_run`).
pub(crate) fn run_program(
    program: &noeta_ast::Program,
    editions: &noeta_lexer::EditionMap,
    sources: &SourceMap,
    args: Vec<String>,
) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `check_all` produces both the gate diagnostics and the `type_of` site map the backend
    // needs, so the checker runs exactly once (it previously ran again inside the backend).
    let checked = noeta_check::check_all_with_editions(program, editions.clone());
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(sources, checked.diagnostics.iter());
        return 1;
    }

    match execute_real_host(program, &checked, args) {
        Ok((result, trace)) => {
            print!("{}", result.stdout);
            let _ = io::stdout().flush();
            emit_diagnostics_mapped(sources, result.diagnostics.iter());
            // An abort's stack trace, after the diagnostic it belongs to. Only when there is a call
            // chain to show — a single-frame trace repeats what the diagnostic's span already says.
            if trace.len() >= 2 {
                eprint!("{}", noeta_vm::render_trace(&trace, sources));
            }
            result.exit_code
        }
        Err(err) => {
            eprintln!("noeta: {err}");
            1
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
pub(crate) fn execute_real_host(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
    args: Vec<String>,
) -> Result<(noeta_backend::RunResult, Vec<noeta_vm::TraceFrame>), String> {
    let (result, trace, _) = run_module_real_host(
        std::sync::Arc::new(compile_real(program, checked)?),
        args,
        false,
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
) -> (
    noeta_backend::RunResult,
    Vec<noeta_vm::TraceFrame>,
    Option<noeta_vm::JitReport>,
) {
    let app_id = p2p_app_namespace(&args);
    noeta_runner::run_module_real_host(module, args, app_id, jit_report)
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
    if let Some(code) = compose::maybe_delegate(file) {
        return code;
    }
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
    match compile_whole_file(file, tiers, target, no_cache) {
        Ok(compiled) => run_compiled_module(
            compiled.module,
            &compiled.sources,
            program_args(file, args),
            jit_stats,
        ),
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
