//! `noeta` — the user-facing toolchain binary.
//!
//! Exposes `run` (execute a file), `test` (run a program's `@test` blocks), `dump` (disassemble to
//! VM bytecode — a debugging aid), and `repl` (interactive); all drive the same pipeline crates, so
//! the binary is thin glue. The binary is
//! named `noeta` (the Noeta toolchain binary). The conformance corpus / differential
//! / leak harness that tests the *implementation* is a separate dev binary (`noeta-conformance`), not
//! a subcommand here — which is what keeps the `noeta test` verb free for a user program's own
//! `@test {}` blocks (object-model slice 6).

// The runtime is allocation-heavy (every heap value — strings, lists, maps, objects — is a boxed
// `Obj`), so the toolchain binary uses mimalloc instead of the system allocator. Correctness is
// unaffected (the leak oracle counts live objects, not allocator behavior); it is a throughput win
// on allocation-bound programs.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use clap::{Parser, Subcommand};
use noeta_ast::{AttrArg, AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_diagnostics::{Diagnostic, DiagnosticCode, render, render_mapped};
use noeta_lexer::{TokenKind, lex};
use noeta_parser::{parse, parse_fragment};
use noeta_span::{Source, SourceId, SourceMap, Span};
use noeta_vm::{SessionOutput, VmBackend, VmSession};

mod manifest;

#[derive(Parser)]
#[command(name = "noeta", version, about = "The Noeta toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a program file.
    Run {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Activate a dev-tier for this run, e.g. `--tier debug` to compile in `@debug { … }`
        /// blocks (object-model slice 6). Repeatable. Without it, every tier block is stripped.
        /// (The interim active-set interface, complementary to `--profile`.)
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a build profile makes live (from `noeta.toml`), e.g.
        /// `--profile dev`. Unioned with any `--tier`.
        #[arg(long)]
        profile: Option<String>,
        /// Bypass the transparent startup cache for this run: don't read a cached compile and don't
        /// write one. Equivalent to setting `NOETA_NO_CACHE`. Recompiles from source regardless.
        #[arg(long)]
        no_cache: bool,
    },
    /// Discover and run a program's `@test` blocks (object-model slice 6).
    Test {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Stop after the first failing test instead of running them all.
        #[arg(long)]
        fail_fast: bool,
        /// Number of tests to run concurrently (default: the machine's parallelism).
        #[arg(long, short)]
        jobs: Option<usize>,
        /// Run only tests tagged `#[Group("<name>")]` with this group.
        #[arg(long)]
        group: Option<String>,
        /// Only run when the `test` tier is live in this `noeta.toml` build profile; otherwise the
        /// runner does nothing.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Discover and run a program's `@bench` blocks, measuring each (object-model slice 6).
    Bench {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Override the iteration count for every benchmark, taking precedence over a per-bench
        /// `@bench(iterations: N)` directive. Without either, a default count is used.
        #[arg(long)]
        iterations: Option<u64>,
        /// Only run when the `bench` tier is live in this `noeta.toml` build profile; otherwise the
        /// runner does nothing.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Extract a program's `@doc { … }` text blocks to stdout (object-model slice 6).
    Doc {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Only extract when the `doc` tier is live in this `noeta.toml` build profile; otherwise
        /// nothing is emitted.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Compile a program to a self-contained `.noeb` bundle (P-AOT L1): the versioned bytecode a
    /// `noeta run app.noeb` executes directly, so a program ships **without its `.noe` source**.
    /// Uses the same compile pipeline as `run`; dev-tier blocks are stripped unless made live by
    /// `--tier`/`--profile`, so a production build never carries `@test`/`@debug`/`@doc` content.
    Build {
        /// Path to the entry `.noe` file.
        file: PathBuf,
        /// Output path (default: the input path with a `.noeb` extension, or — with `--exe` — with
        /// its extension stripped, e.g. `app.noe` → `app`).
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Emit a self-contained executable (P-AOT L2) instead of a `.noeb`: the bundle is stapled
        /// onto a copy of this runtime binary, so the artifact runs the program on its own with no
        /// separate `.noeb` or interpreter alongside it.
        #[arg(long)]
        exe: bool,
        /// Emit a **native** executable (P-AOT L3): every eligible prototype is compiled ahead of
        /// time to machine code and linked into the binary (the rest interpret), then the bundle is
        /// stapled on as with `--exe`. Requires a C toolchain (`cc`); the AOT runtime archive is
        /// located via `NOETA_AOT_RUNTIME_LIB`, else built from the workspace (interim).
        #[arg(long)]
        native: bool,
        /// Activate a dev-tier for this build, e.g. `--tier debug`. Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Disassemble a program to its VM bytecode (a debugging aid: shows the exact opcodes,
    /// constants, shapes, and method tables `noeta run` executes). Compiled with the same pipeline
    /// as `run`, so the output reflects what actually runs.
    Dump {
        /// Path to a `.noe` file.
        file: PathBuf,
        /// Activate a dev-tier before disassembling (as `noeta run --tier …`). Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Statically check a program without running or building it: parse every `.noe` file and verify
    /// it type-checks, reporting all diagnostics (the `cargo check` / `tsc --noEmit` primitive). Uses
    /// the same load → link → type-check pipeline as `run`, then stops before codegen. Exits non-zero
    /// if any error-severity diagnostic is found; warnings print but do not fail.
    Check {
        /// File or directory to check (default: the current directory, walked recursively for
        /// `.noe` files). A directory checks every file it contains; a file checks just that one
        /// (with its directory-sibling modules linked in, as `run` does).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Activate a dev-tier before checking, e.g. `--tier debug` to check inside `@debug { … }`
        /// blocks (as `noeta run --tier …`). Repeatable.
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Start an interactive REPL. Entries type-check before running (an entry with a type error
    /// prints its `E0xxx` diagnostics and is skipped) — the default since session-checker C2/C5.
    Repl {
        /// Skip per-entry type checking (the pre-C2 behavior: every well-parsed entry runs, type
        /// errors surface at run time). Also toggleable at the prompt with `:check on` / `:check off`.
        #[arg(long)]
        no_check: bool,
        /// Run this program to completion first (fully checked, imports resolved), then open the
        /// prompt with everything it declared and bound live — a bootstrapped session ("tinker"):
        /// a framework's bootstrap script gives an app-context REPL. A bootstrap that fails to
        /// load, check, or run exits with its diagnostics instead of opening a broken prompt.
        #[arg(long, value_name = "FILE")]
        load: Option<PathBuf>,
    },
    /// Run the Noeta language server over stdio (LSP). Started by an editor client (e.g. the
    /// VS Code extension); speaks JSON-RPC on stdin/stdout. Provides live diagnostics, hover
    /// types, and navigation over the compiler's incremental query graph.
    Lsp,
    /// Run the Noeta debug adapter over stdio (DAP). Started by an editor's debug UI; speaks the
    /// Debug Adapter Protocol on stdin/stdout. Runs a program under the production VM (JIT unarmed
    /// for full introspection) with breakpoints, stepping, and variable inspection.
    Dap,
    /// Serve a program's HTTP handler. The file defines a top-level `fn fetch(req: Request):
    /// Response` (sync or async) and `use std.{http}`; `noeta serve` runs the file's top-level
    /// setup, then binds a listener and drives the handler — the ergonomic entry point over an
    /// explicit `http.serve(...)` call. Runs until interrupted (Ctrl-C). Single worker,
    /// cooperatively concurrent (a slow async handler yields while others progress); multi-core
    /// worker isolates are a follow-on.
    Serve {
        /// Path to a `.noe` file exporting a `fetch` handler.
        file: PathBuf,
        /// The TCP port to bind (default 8080); the listener binds all interfaces (`0.0.0.0`).
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

fn main() -> ExitCode {
    // P-AOT L2: if this executable is a `noeta build --exe` artifact (a bundle stapled onto a copy
    // of the runtime), run the embedded program directly — the shipped app is not the toolchain, so
    // its CLI verbs are irrelevant. A plain `noeta` binary has no trailer and falls through to the
    // normal CLI. Detection reads only the tail of the file, not the whole binary.
    if let Some(code) = try_run_stapled() {
        return code;
    }
    match Cli::parse().command {
        Command::Run {
            file,
            tier,
            profile,
            no_cache,
        } => cmd_run(&file, &tier, &profile, no_cache),
        Command::Test {
            file,
            fail_fast,
            jobs,
            group,
            profile,
        } => cmd_test(&file, fail_fast, jobs, &group, &profile),
        Command::Bench {
            file,
            iterations,
            profile,
        } => cmd_bench(&file, iterations, &profile),
        Command::Doc { file, profile } => cmd_doc(&file, &profile),
        Command::Build {
            file,
            out,
            exe,
            native,
            tier,
            profile,
        } => cmd_build(&file, out.as_deref(), exe, native, &tier, &profile),
        Command::Dump {
            file,
            tier,
            profile,
        } => cmd_dump(&file, &tier, &profile),
        Command::Check {
            path,
            tier,
            profile,
        } => cmd_check(&path, &tier, &profile),
        Command::Repl { no_check, load } => cmd_repl(!no_check, load),
        Command::Lsp => cmd_lsp(),
        Command::Dap => cmd_dap(),
        Command::Serve { file, port } => cmd_serve(&file, port),
    }
}

/// Start the Noeta language server over stdio, blocking until the editor client disconnects.
fn cmd_lsp() -> ExitCode {
    noeta_lsp::run_stdio();
    ExitCode::SUCCESS
}

/// Start the Noeta debug adapter over stdio, blocking until the editor client disconnects.
fn cmd_dap() -> ExitCode {
    noeta_dap::run_stdio();
    ExitCode::SUCCESS
}

/// For a tier runner: whether its `tier` is live under `--profile`. `Ok(true)` when no profile was
/// given (the runner always runs); `Ok(false)` when a profile was given but does not make `tier`
/// live (the runner should no-op); `Err` on a profile-resolution failure (a fatal error the caller
/// prints).
fn tier_active_in_profile(
    entry: &std::path::Path,
    profile: &Option<String>,
    tier: &str,
) -> Result<bool, String> {
    match profile {
        None => Ok(true),
        Some(name) => Ok(manifest::resolve_active_tiers(entry, name)?
            .iter()
            .any(|t| t == tier)),
    }
}

/// Type-check and run a program, writing stdout to the real stdout and rendering any diagnostics to
/// stderr — each against the source its span belongs to (via the `SourceMap`). Returns the process
/// exit code. `program` is the loaded program, possibly after dev-tier activation (`cmd_run`).
fn run_program(program: &noeta_ast::Program, sources: &SourceMap) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `check_all` produces both the gate diagnostics and the `type_of` site map the backend
    // needs, so the checker runs exactly once (it previously ran again inside the backend).
    let checked = noeta_check::check_all(program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(sources, checked.diagnostics.iter());
        return 1;
    }

    match execute_real_host(program, &checked) {
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
            eprintln!("lang: {err}");
            1
        }
    }
}

/// Compile an already-typechecked program straight to a bytecode [`Module`] for the real (VM)
/// execution path (isolates I.4a). This runs the *same* Core-IR lowering + precise-RC drop + reuse
/// passes the differential and the eval reference use, then continues IR → bytecode (the extra stage
/// the VM needs); reusing `checked`'s site maps keeps the checker to a single run.
///
/// Every program that parses and type-checks compiles to bytecode — the differential holds the VM at
/// 100% coverage *by construction* (each language feature lands in both backends together). So an
/// `Err` here does not mean "ordinary unsupported user program"; it means that invariant broke, and
/// we surface it rather than silently downgrading to a different backend.
fn compile_real(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<noeta_bytecode::Module, String> {
    noeta_compiler::compile_with_sites(
        program,
        checked.sites.clone(),
        // Real execution runs isolates on OS threads (I.4b): lower `isolate f(args)` to `SpawnIsolate`.
        // The differential/salsa paths pass false (byte-identical cooperative sandbox).
        true,
        // `noeta run` is a production compile — no debug info (the debugger's `noeta dap` compiles
        // the same program with debug = true).
        false,
    )
    .map_err(|u| {
        format!(
            "internal error: the VM cannot compile this program: {}",
            u.reason
        )
    })
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
fn execute_real_host(
    program: &noeta_ast::Program,
    checked: &noeta_check::Checked,
) -> Result<(noeta_backend::RunResult, Vec<noeta_vm::TraceFrame>), String> {
    Ok(run_module_real_host(std::sync::Arc::new(compile_real(
        program, checked,
    )?)))
}

/// Run an already-compiled [`Module`] against the real host — the shared execution core of
/// [`execute_real_host`] (source path) and the `.noeb` bundle runner (P-AOT L1.2), which loads a
/// module directly with no source to compile.
fn run_module_real_host(
    module: std::sync::Arc<noeta_bytecode::Module>,
) -> (noeta_backend::RunResult, Vec<noeta_vm::TraceFrame>) {
    // A factory that mints a fresh real host + wall-clock executor per isolate (isolates I.4b): the
    // main program gets one, and each real-thread `isolate f(args)` gets its own so a worker's disk /
    // clock / async state is independent. Injected here (not in `noeta-vm`) so the VM crate needs no
    // `noeta-runtime`/tokio dependency. A worker that cannot start its runtime panics the worker thread,
    // which surfaces as an isolate failure at the `.await`.
    let factory: noeta_vm::IsolateFactory = std::sync::Arc::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_runtime::RealHost::new().expect("cannot start an isolate's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_runtime::RealExecutor::new().expect("cannot start an isolate's async executor"),
        );
        (host, executor)
    });
    let (host, executor) = factory();
    // Real isolates run on OS threads (out-of-oracle); channel-shipping isolates fall back to
    // cooperative tasks (cross-thread channels are I.4c). The differential keeps the sandbox pair.
    VmBackend::new().run_module_with_host_and_executor_parallel(module, host, executor, factory)
}

/// P-AOT L2: detect and run a bundle stapled onto this executable (a `noeta build --exe` artifact),
/// returning its exit code — or `None` when this is the plain toolchain binary (no trailer), so the
/// normal CLI runs. Reads only the trailer + embedded blob, seeking rather than slurping the whole
/// binary, so the toolchain's own startup is not taxed on the common (no-bundle) path. Any IO/format
/// hiccup is treated as "no bundle" (returns `None`): the toolchain must still start normally.
fn try_run_stapled() -> Option<ExitCode> {
    use std::io::{Read, Seek, SeekFrom};

    let exe_path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe_path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    let trailer_len = noeta_bundle::TRAILER_LEN as u64;
    if size < trailer_len {
        return None;
    }
    // Read the fixed trailer at the tail; a missing sentinel means this is the plain binary.
    file.seek(SeekFrom::End(-(trailer_len as i64))).ok()?;
    let mut trailer = [0u8; noeta_bundle::TRAILER_LEN];
    file.read_exact(&mut trailer).ok()?;
    let blob_len = noeta_bundle::stapled_len(&trailer)?;
    let blob_start = size.checked_sub(trailer_len + blob_len as u64)?;
    file.seek(SeekFrom::Start(blob_start)).ok()?;
    let mut blob = vec![0u8; blob_len];
    file.read_exact(&mut blob).ok()?;
    Some(cmd_run_bundle(&exe_path, &blob))
}

fn emit_diagnostics<'a>(source: &Source, diagnostics: impl Iterator<Item = &'a Diagnostic>) {
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let _ = stderr.write_all(render(source, diagnostic).as_bytes());
    }
}

/// Print [`noeta_diagnostics::render_mapped`]'s cross-module rendering to stderr — each diagnostic
/// resolved against the source its span belongs to.
fn emit_diagnostics_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) {
    let _ = io::stderr().write_all(render_mapped(sources, diagnostics).as_bytes());
}

fn cmd_run(
    file: &std::path::Path,
    tiers: &[String],
    profile: &Option<String>,
    no_cache: bool,
) -> ExitCode {
    // P-AOT L1.2: a `.noeb` bundle runs directly — no source, no compile. Sniff the magic (cheap,
    // and we need the bytes to load it anyway); anything else is source, handled below. Tiers are a
    // *build*-time concern (they are already baked into the bundle), so `--tier`/`--profile` on a
    // bundle run are meaningless — reject them rather than silently ignore.
    if let Ok(bytes) = std::fs::read(file)
        && noeta_bundle::is_bundle(&bytes)
    {
        if !tiers.is_empty() || profile.is_some() {
            eprintln!(
                "lang: --tier/--profile apply at build time; a .noeb bundle is already built"
            );
            return ExitCode::from(2);
        }
        return cmd_run_bundle(file, &bytes);
    }

    // Everything else — resolve tiers, consult the startup cache, and (on a miss) load → check →
    // compile — is the shared whole-file pipeline. On success run the module; on failure report it.
    match compile_whole_file(file, tiers, profile, no_cache) {
        Ok(compiled) => run_compiled_module(compiled.module, &compiled.sources),
        Err(failure) => failure.report(),
    }
}

/// `noeta check [PATH]` — statically validate source without running or building it: parse every
/// `.noe` file and verify it type-checks, printing all diagnostics and exiting non-zero if any is an
/// error. This is `cmd_run`'s front half — load → (activate tiers) → `check_all` — stopping before
/// `execute_real_host`, so it has no side effects.
///
/// `PATH` (default `.`) is a file or a directory; a directory is walked recursively and **every**
/// `.noe` file is checked as its own entry. The loader links only an entry's *directory-sibling*
/// modules (there is no cross-directory module graph), so checking each file as an entry is what
/// guarantees a library module no single entry imports is still parsed and type-checked. A module
/// shared by several entries is therefore linked (and its diagnostics produced) once per importer;
/// diagnostics are deduplicated globally by their source file + span + code so each is reported once.
fn cmd_check(path: &std::path::Path, tiers: &[String], profile: &Option<String>) -> ExitCode {
    use noeta_diagnostics::Severity;

    // The active tier set — resolved once and applied to every file — is the union of a `--profile`'s
    // live tiers (from `noeta.toml`) and any explicit `--tier` flags, exactly as `cmd_run` resolves
    // it. A bad profile fails fast before any file is read.
    let mut active: Vec<String> = match profile {
        Some(name) => match manifest::resolve_active_tiers(path, name) {
            Ok(tiers) => tiers,
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        },
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }
    let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();

    // The set of entry files to check: the file itself, or every `.noe` file under the directory.
    let entries: Vec<PathBuf> = if path.is_dir() {
        noe_files(path)
    } else {
        vec![path.to_path_buf()]
    };
    if entries.is_empty() {
        eprintln!("lang: no `.noe` files found under `{}`", path.display());
        return ExitCode::from(2);
    }

    // Deduplicate diagnostics across every entry's workspace. `SourceId`s are workspace-local (each
    // load restarts them at 0), so the key is the *file name* the diagnostic renders against plus its
    // byte span and code — never the id. The map's key order (name, then offset, then code) is also
    // the render order, so output is deterministic. Value keeps the owning `Source` so each renders
    // against the right file with the single-source renderer.
    let mut diags: std::collections::BTreeMap<(String, u32, u32, &'static str), (Source, Diagnostic)> =
        std::collections::BTreeMap::new();
    let mut fold = |source: &Source, diag: &Diagnostic| {
        let key = (
            source.name().to_string(),
            diag.span.start,
            diag.span.end,
            diag.code.code(),
        );
        diags
            .entry(key)
            .or_insert_with(|| (source.clone(), diag.clone()));
    };

    let mut unreadable = false;
    for entry in &entries {
        match noeta_loader::load(entry) {
            Err(err) => {
                // One unreadable file does not abort the whole run — record it and keep checking the
                // rest, so `check` reports as much as it can in a single pass.
                eprintln!("lang: cannot read {}: {err}", entry.display());
                unreadable = true;
            }
            Ok(Err(load_diagnostics)) => {
                // Lex/parse errors — each already carries the source it renders against.
                for ld in &load_diagnostics {
                    fold(&ld.source, &ld.diagnostic);
                }
            }
            Ok(Ok(linked)) => {
                // Activate the resolved dev-tiers before checking, as `run`/`build`/`dump` do; with no
                // active tiers the program is checked as-is. Tier-activation diagnostics resolve
                // against the same workspace sources.
                if active_refs.is_empty() {
                    for d in &noeta_check::check_all(&linked.program).diagnostics {
                        fold(linked.sources.source(d.span.source), d);
                    }
                } else {
                    let activated = noeta_check::activate_tiers(&linked.program, &active_refs);
                    for d in &activated.diagnostics {
                        fold(linked.sources.source(d.span.source), d);
                    }
                    for d in &noeta_check::check_all(&activated.program).diagnostics {
                        fold(linked.sources.source(d.span.source), d);
                    }
                }
            }
        }
    }

    // Render every unique diagnostic against its own source (single-source renderer, color disabled),
    // then a summary line. Errors gate the exit code; warnings/notes print but pass.
    let mut stderr = io::stderr();
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for (source, diag) in diags.values() {
        match diag.severity {
            Severity::Error => errors += 1,
            Severity::Warning => warnings += 1,
            Severity::Note => {}
        }
        let _ = stderr.write_all(render(source, diag).as_bytes());
    }
    let n = entries.len();
    let files = if n == 1 { "file" } else { "files" };
    eprintln!("checked {n} {files}: {errors} error(s), {warnings} warning(s)");

    if errors > 0 {
        ExitCode::from(1)
    } else if unreadable {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Collect every `.noe` file under `root`, recursively, in sorted order (so discovery and thus the
/// check order are deterministic). Hand-rolled in the style of the loader's `read_siblings` — a
/// depth-first `read_dir` walk that silently skips directories it cannot read (a partial tree still
/// checks what it can). Symlinked directories are followed by `read_dir` as ordinary entries; cycles
/// are not guarded against, matching the loader's own assumptions about a normal source tree.
fn noe_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|ext| ext == "noe") {
                out.push(p);
            }
        }
        stack.extend(dirs);
    }
    out.sort();
    out
}

/// `noeta serve <FILE> [--port N]` — the ergonomic HTTP-server entry point (http-server S4). The
/// file exports a top-level `fn fetch(req: Request): Response` (sync or async) and `use std.{http}`;
/// this runs the file's top-level setup, then synthesizes and runs `http.serve(<port>, fetch)`,
/// which binds `0.0.0.0:<port>` and drives the handler over the real host until interrupted
/// (Ctrl-C). Single worker, cooperatively concurrent — a slow async handler yields while others
/// progress. (Multi-core worker isolates are a follow-on; see `plans/http-server`.) Layering the
/// serve call on top of the loaded program means the mechanism is the exact same `http.serve` a
/// program can call directly — the command only supplies the entry convention and the port.
fn cmd_serve(file: &std::path::Path, port: u16) -> ExitCode {
    use noeta_ast::{Expr, Stmt};
    use noeta_span::Span;

    let mut linked = match noeta_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return ExitCode::from(1);
        }
        Ok(Ok(linked)) => linked,
    };

    // Synthesize `http.serve(<port>, fetch)` as a trailing top-level statement. The program supplies
    // `fetch` and `use std.{http}` (any handler builds responses with `http.response`, so `http` is
    // already imported); a missing `fetch`/`http` surfaces as an ordinary check error. A synthetic
    // span (offset 0) is fine — this node is compiler-generated, never the subject of a diagnostic
    // the user needs to locate.
    let sp = Span::empty_at(0);
    let ident = |name: &str| Expr::Ident {
        name: name.to_string(),
        span: sp,
    };
    let serve = Expr::Member {
        receiver: Box::new(ident("http")),
        name: "serve".to_string(),
        name_span: sp,
        span: sp,
    };
    let call = Expr::Call {
        callee: Box::new(serve),
        args: vec![
            Expr::Int {
                value: i64::from(port),
                span: sp,
            },
            ident("fetch"),
        ],
        span: sp,
    };
    linked.program.stmts.push(Stmt::Expr {
        expr: call,
        span: sp,
    });

    eprintln!("noeta serve: listening on http://0.0.0.0:{port} (Ctrl-C to stop)");
    // `serve` injects an `http.serve(...)` call into the program before compiling, so its module
    // differs from `run`'s for the same source — it must never share the startup cache's
    // `(source+tiers)` key, and is left uncached (and long-lived, so it barely benefits anyway).
    exit_code(run_program(&linked.program, &linked.sources))
}

/// Run a `.noeb` bundle (P-AOT L1.2): decode the module from the versioned container and execute it
/// on the real host, exactly as a source run does after compiling — but with no source to compile
/// or type-check (both happened at build time). A runtime abort's diagnostics/trace carry spans but
/// the bundle ships no source text, so they render against a synthetic empty source (message + code
/// + location show; no code snippet) — the honest cost of a source-free artifact.
fn cmd_run_bundle(file: &std::path::Path, bytes: &[u8]) -> ExitCode {
    let module = match noeta_bundle::read(bytes) {
        Ok(module) => module,
        Err(err) => {
            eprintln!("lang: cannot load {}: {err}", file.display());
            return ExitCode::from(2);
        }
    };
    let sources = SourceMap::new(vec![Source::new(
        SourceId::FIRST,
        file.display().to_string(),
        "",
    )]);
    run_compiled_module(std::sync::Arc::new(module), &sources)
}

/// Run an already-compiled [`Module`] on the real host and render its output — the shared tail of the
/// `.noeb` bundle runner and the startup-cache *hit* path (both have a module with no source to
/// compile). Diagnostics/trace render against `sources` (real workspace sources on a cache hit; a
/// synthetic empty source for a source-free bundle).
/// A resolved startup-cache slot: an open cache, the content key for this program, and the workspace
/// `SourceMap` (so a cache hit renders diagnostics against real source without re-parsing). Built by
/// [`open_startup_cache`], consumed by [`compile_whole_file`].
struct CacheSlot {
    cache: noeta_cache::Cache,
    key: noeta_cache::CacheKey,
    sources: SourceMap,
}

/// A compiled whole-file program: the runnable module plus the sources its spans resolve against.
struct Compiled {
    module: std::sync::Arc<noeta_bytecode::Module>,
    sources: SourceMap,
}

/// A whole-file compile failure, carrying what's needed to render it. [`report`](Self::report)
/// prints it and yields the process exit code, matching each command's prior behavior.
enum CompileFailure {
    /// A message rendered as `lang: {0}` with exit 1 (profile resolution / compiler-internal error).
    Message(String),
    /// The entry file could not be read (exit 2).
    Unreadable(String),
    /// Load-time (lex/parse) diagnostics, each paired with its own source (exit 1).
    Load(Vec<noeta_loader::LoadDiagnostic>),
    /// Tier-activation or type-check diagnostics, rendered against `sources` (exit 1).
    Diagnostics {
        sources: SourceMap,
        diagnostics: Vec<Diagnostic>,
    },
}

impl CompileFailure {
    /// Print the failure to stderr and return the process exit code.
    fn report(&self) -> ExitCode {
        match self {
            CompileFailure::Message(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(1)
            }
            CompileFailure::Unreadable(msg) => {
                eprintln!("lang: {msg}");
                ExitCode::from(2)
            }
            CompileFailure::Load(diagnostics) => {
                let mut stderr = io::stderr();
                for ld in diagnostics {
                    let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
                }
                ExitCode::from(1)
            }
            CompileFailure::Diagnostics {
                sources,
                diagnostics,
            } => {
                emit_diagnostics_mapped(sources, diagnostics.iter());
                ExitCode::from(1)
            }
        }
    }
}

/// The whole-file compile pipeline, shared by `run`/`dump`/`build` and cache-aware in one place: any
/// command that wants "a source file → its runnable [`Module`]" goes through here, so the startup
/// cache is applied exactly once rather than wired per command.
///
/// Resolves the active tier set (profile ∪ `--tier`), then consults the startup cache: on a **hit**
/// the decoded module is returned directly (the whole front-end is skipped); on a **miss** it loads →
/// activates tiers → type-checks → compiles, populates the cache (best-effort), and returns. Because
/// `run`/`dump`/`build` all compile identically (same [`compile_real`]), they share cache entries —
/// a `noeta build` warms the exact entry `noeta run` reads, and vice versa.
///
/// `serve` does **not** use this (it injects an `http.serve(...)` call, so its module differs for the
/// same source and must never share the `(source+tiers)` key). `test`/`bench` do **not** either (they
/// compile a separate module per `@test`/`@bench` case — a different granularity).
fn compile_whole_file(
    file: &std::path::Path,
    tiers: &[String],
    profile: &Option<String>,
    no_cache: bool,
) -> Result<Compiled, CompileFailure> {
    // The active tier set is the union of any `--profile`'s live tiers (from `noeta.toml`) and any
    // explicit `--tier` flags, resolved before loading so a bad profile fails fast.
    let mut active: Vec<String> = match profile {
        Some(name) => manifest::resolve_active_tiers(file, name).map_err(CompileFailure::Message)?,
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }

    // Startup cache (M3): on a hit, return the cached module — load/check/compile all skipped.
    let cache = open_startup_cache(file, &active, no_cache);
    if let Some(slot) = &cache
        && let Some(blob) = slot.cache.load(&slot.key)
        && let Ok(module) = noeta_bundle::read(&blob)
    {
        return Ok(Compiled {
            module: std::sync::Arc::new(module),
            sources: slot.sources.clone(),
        });
    }

    // Miss: load + link (sibling `.noe` modules the entry `use`s are resolved and merged; a lone file
    // links to itself), activate any dev-tiers, type-check, and compile to bytecode.
    let linked = match noeta_loader::load(file) {
        Err(err) => {
            return Err(CompileFailure::Unreadable(format!(
                "cannot read {}: {err}",
                file.display()
            )));
        }
        Ok(Err(load_diagnostics)) => return Err(CompileFailure::Load(load_diagnostics)),
        Ok(Ok(linked)) => linked,
    };
    let sources = linked.sources;
    // Activation inlines each `@<tier> { … }` block; with no active tiers the program runs as-is and
    // every tier block is stripped at lowering (the default). Activation is only done when needed.
    let program = if active.is_empty() {
        linked.program
    } else {
        let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();
        let activated = noeta_check::activate_tiers(&linked.program, &active_refs);
        if !activated.diagnostics.is_empty() {
            return Err(CompileFailure::Diagnostics {
                sources,
                diagnostics: activated.diagnostics,
            });
        }
        activated.program
    };
    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return Err(CompileFailure::Diagnostics {
            sources,
            diagnostics: checked.diagnostics,
        });
    }
    let module = match compile_real(&program, &checked) {
        Ok(module) => std::sync::Arc::new(module),
        Err(err) => return Err(CompileFailure::Message(err)),
    };

    // Populate the cache, best-effort. Synchronous: encoding + writing the blob is trivial next to
    // the compile we just paid, and a failure (read-only dir, disk full) never touches the outcome.
    if let Some(slot) = &cache {
        let _ = slot.cache.store(&slot.key, &noeta_bundle::write(&module));
    }
    Ok(Compiled { module, sources })
}

fn run_compiled_module(
    module: std::sync::Arc<noeta_bytecode::Module>,
    sources: &SourceMap,
) -> ExitCode {
    let (result, trace) = run_module_real_host(module);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    emit_diagnostics_mapped(sources, result.diagnostics.iter());
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, sources));
    }
    exit_code(result.exit_code)
}

/// Build the startup-cache slot for a source run: open the cache and compute the content key from
/// the raw workspace (entry + sibling module sources) + runtime version + binary identity + the
/// active tier set. Returns `None` — meaning "run uncached" — when caching is disabled
/// (`--no-cache` or `NOETA_NO_CACHE`), the running binary can't be identified (so freshness can't be
/// guaranteed), the entry can't be read, or the cache directory can't be opened.
fn open_startup_cache(
    file: &std::path::Path,
    active: &[String],
    no_cache: bool,
) -> Option<CacheSlot> {
    if no_cache || std::env::var_os("NOETA_NO_CACHE").is_some() {
        return None;
    }
    // The binary's build identity is mandatory: without it a same-version local toolchain rebuild
    // would reuse stale bytecode. If we can't obtain it, we must not cache.
    let binary = noeta_cache::binary_identity()?;
    // Read the entry + sibling sources (no lex/parse) — both the key material and, on a hit, the
    // SourceMap for rendering. SourceIds here match `noeta_loader::load`'s assignment (entry = 0,
    // sorted siblings 1..), so a cached module's spans resolve correctly against this map.
    let workspace = noeta_loader::read_workspace(file).ok()?;
    let mut key = noeta_cache::KeyBuilder::new();
    key.source(
        source_key_name(&workspace.entry),
        workspace.entry.text().as_bytes(),
    );
    for module in &workspace.modules {
        key.source(source_key_name(module), module.text().as_bytes());
    }
    key.runtime_version(noeta_bundle::RUNTIME_VERSION)
        .binary_identity(binary);
    for tier in active {
        key.tier(tier);
    }
    let key = key.finish();

    let cache = noeta_cache::Cache::open()?;
    let mut sources = Vec::with_capacity(1 + workspace.modules.len());
    sources.push(workspace.entry);
    sources.extend(workspace.modules);
    Some(CacheSlot {
        cache,
        key,
        sources: SourceMap::new(sources),
    })
}

/// The cache-key name for a source: its file name, so the key is independent of the path the program
/// was invoked through (`./app.noe` and `app.noe` share an entry). Two distinct programs can only
/// share a name here if their whole hashed content also matches — in which case they are identical.
fn source_key_name(source: &Source) -> &str {
    std::path::Path::new(source.name())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| source.name())
}

/// `noeta dump <FILE>` — disassemble the program to its VM bytecode and print it to stdout. Loads,
/// activates any `--tier`/`--profile`, type-checks, and compiles through the **same** pipeline as
/// `noeta run` (`compile_real`), so the disassembly is exactly what the VM executes — the tool for
/// inspecting codegen (which ops a construct lowers to, whether a reuse/in-place fast path fired,
/// how names/constants are laid out). A type error prints diagnostics and exits non-zero, like `run`.
fn cmd_dump(file: &std::path::Path, tiers: &[String], profile: &Option<String>) -> ExitCode {
    // Same whole-file compile as `run` (so the disassembly is exactly what the VM runs), and a cache
    // participant — a cached module is byte-identical to a fresh compile, so the disassembly matches.
    match compile_whole_file(file, tiers, profile, false) {
        Ok(compiled) => {
            print!("{}", compiled.module.disassemble());
            let _ = io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(failure) => failure.report(),
    }
}

/// `noeta build <FILE>` — compile a program to a self-contained artifact (P-AOT L1/L2). Loads +
/// links, activates any `--tier`/`--profile`, type-checks, and compiles through the **same**
/// `compile_real` pipeline as `run`/`dump`. The result is emitted either as a `.noeb` bundle
/// (`noeta_bundle::write`, run by `noeta run app.noeb`) or — with `--exe` — as a self-contained
/// executable that runs the program on its own (`emit_exe`). Either artifact carries no `.noe`
/// source. A type error prints diagnostics and exits non-zero, like `run`.
fn cmd_build(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    exe: bool,
    native: bool,
    tiers: &[String],
    profile: &Option<String>,
) -> ExitCode {
    if exe && native {
        eprintln!("lang: --exe and --native are mutually exclusive");
        return ExitCode::from(2);
    }
    // Same whole-file compile + startup cache as `run`/`dump`. The emit format doesn't affect the
    // module, so a `build` shares cache entries with a `run` of the same source — each warms the other.
    let module = match compile_whole_file(file, tiers, profile, false) {
        Ok(compiled) => compiled.module,
        Err(failure) => return failure.report(),
    };
    let module = module.as_ref();
    if native {
        emit_native(file, out, module)
    } else if exe {
        emit_exe(file, out, module)
    } else {
        emit_bundle(file, out, module)
    }
}

/// Emit `module` as a standalone `.noeb` bundle (P-AOT L1.2) — the default `noeta build` output.
/// Writes to `out` if given, else the input path with a `.noeb` extension.
fn emit_bundle(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let blob = noeta_bundle::write(module);
    let out_path = out
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("noeb"));
    match std::fs::write(&out_path, &blob) {
        Ok(()) => {
            eprintln!("wrote {} ({} bytes)", out_path.display(), blob.len());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("lang: cannot write {}: {err}", out_path.display());
            ExitCode::from(2)
        }
    }
}

/// Emit `module` as a self-contained executable (P-AOT L2.1): staple its bundle onto a copy of this
/// runtime binary, so the artifact runs the program with no separate `.noeb` or interpreter. Writes
/// to `out` if given, else the input path with its extension stripped (`app.noe` → `app`, or `.exe`
/// on Windows). The artifact is marked executable on Unix.
fn emit_exe(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    // The runtime image to embed is *this* binary — the toolchain the user invoked. (A stapled exe
    // never reaches `build`; it runs its bundle, so `current_exe` here is always the plain toolchain.)
    let runtime = match std::env::current_exe().and_then(std::fs::read) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: cannot read the runtime binary to embed: {err}");
            return ExitCode::from(2);
        }
    };
    let blob = noeta_bundle::write(module);
    let image = noeta_bundle::staple(&runtime, &blob);

    let default_out = if cfg!(windows) {
        file.with_extension("exe")
    } else {
        file.with_extension("")
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or(default_out);
    // Never clobber the source with the artifact (e.g. an extension-less entry building to itself).
    if out_path == file {
        eprintln!(
            "lang: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("lang: cannot write {}: {err}", out_path.display());
        return ExitCode::from(2);
    }
    // Make the artifact runnable (Unix): rwxr-xr-x.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!("lang: cannot mark {} executable: {err}", out_path.display());
            return ExitCode::from(2);
        }
    }
    eprintln!(
        "wrote {} ({} bytes, self-contained)",
        out_path.display(),
        image.len()
    );
    ExitCode::SUCCESS
}

/// Emit `module` as a **native** executable (P-AOT L3.2b(3)) — the final level. Steps:
///   1. AOT-compile every eligible prototype to a relocatable object (`compile_module_aot`), which
///      also defines the `noeta_aot_dispatch` table.
///   2. Link that object against the AOT runtime staticlib (`libnoeta_aot.a`) with a C toolchain
///      (`cc`) into a native binary: the runtime provides `main` + the `noeta_jit_*` helpers, the
///      object provides the native bodies + the dispatch table, and the linker resolves it all.
///   3. Staple the program's bundle onto that binary (the L2 mechanism), so at startup the runtime
///      recovers the module and binds the linked-in native bodies through the dispatch table.
///
/// The eligible prototypes run as machine code; ineligible ones interpret the same bytecode from the
/// stapled bundle — the identical hybrid the runtime JIT uses, just resolved at build time.
#[cfg(feature = "jit")]
fn emit_native(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let default_out = if cfg!(windows) {
        file.with_extension("exe")
    } else {
        file.with_extension("")
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or(default_out);
    if out_path == file {
        eprintln!(
            "lang: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }

    // 1. AOT object.
    let object = match noeta_vm::compile_module_aot(module) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: AOT compile failed: {err}");
            return ExitCode::from(1);
        }
    };

    // A per-invocation scratch dir for the object + the pre-staple linked binary.
    let work = std::env::temp_dir().join(format!("noeta-aot-{}", std::process::id()));
    if let Err(err) = std::fs::create_dir_all(&work) {
        eprintln!("lang: cannot create a build directory: {err}");
        return ExitCode::from(2);
    }
    let cleanup = |code: ExitCode| -> ExitCode {
        let _ = std::fs::remove_dir_all(&work);
        code
    };
    let obj_path = work.join("program.o");
    if let Err(err) = std::fs::write(&obj_path, &object) {
        eprintln!("lang: cannot write the AOT object: {err}");
        return cleanup(ExitCode::from(2));
    }

    // 2. Locate the runtime archive + the system libs it must link with, then `cc`-link.
    let (archive, libs) = match resolve_aot_runtime() {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("lang: {err}");
            return cleanup(ExitCode::from(1));
        }
    };
    let linked = work.join("linked");
    if let Err(err) = link_native(&obj_path, &archive, &libs, &linked) {
        eprintln!("lang: link failed: {err}");
        return cleanup(ExitCode::from(1));
    }

    // 3. Staple the bundle onto the linked binary (L2), so the runtime recovers the module.
    let runtime = match std::fs::read(&linked) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("lang: cannot read the linked binary: {err}");
            return cleanup(ExitCode::from(2));
        }
    };
    let blob = noeta_bundle::write(module);
    let image = noeta_bundle::staple(&runtime, &blob);
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("lang: cannot write {}: {err}", out_path.display());
        return cleanup(ExitCode::from(2));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!("lang: cannot mark {} executable: {err}", out_path.display());
            return cleanup(ExitCode::from(2));
        }
    }
    eprintln!(
        "wrote {} ({} bytes, native AOT)",
        out_path.display(),
        image.len()
    );
    cleanup(ExitCode::SUCCESS)
}

/// Native AOT (`noeta build --native`) needs the JIT codegen; an interpreter-only build
/// (`--no-default-features`) has no AOT compiler, so it reports that rather than emitting a binary.
#[cfg(not(feature = "jit"))]
fn emit_native(
    _file: &std::path::Path,
    _out: Option<&std::path::Path>,
    _module: &noeta_bytecode::Module,
) -> ExitCode {
    eprintln!("lang: native AOT (`--native`) requires the JIT-enabled build (default features)");
    ExitCode::from(2)
}

/// Locate the AOT runtime staticlib (`libnoeta_aot.a`) and the native system libraries it must be
/// linked against.
/// Priority: an explicit `NOETA_AOT_RUNTIME_LIB` (paired with `NOETA_AOT_LINK_LIBS`,
/// space-separated) — the packaged/hermetic path — else build it from the workspace with `cargo
/// rustc … --print native-static-libs` (interim: needs cargo + the source tree), which both produces
/// the archive and prints the exact link line. Packaging the archive for a shipped toolchain (so
/// `--native` works outside the workspace) is a later distribution decision.
#[cfg(feature = "jit")]
fn resolve_aot_runtime() -> Result<(std::path::PathBuf, Vec<String>), String> {
    if let Ok(path) = std::env::var("NOETA_AOT_RUNTIME_LIB") {
        let libs = std::env::var("NOETA_AOT_LINK_LIBS")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(default_native_libs);
        return Ok((std::path::PathBuf::from(path), libs));
    }

    // Interim workspace build: one `cargo rustc` compiles the staticlib and prints its
    // native-static-libs note. Combined stdout+stderr is captured so we can parse the note.
    let output = std::process::Command::new("cargo")
        .args([
            "rustc",
            "-p",
            "noeta-aot-runtime",
            "--release",
            "--",
            "--print",
            "native-static-libs",
        ])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run cargo to build the AOT runtime: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "building the AOT runtime failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let notes = String::from_utf8_lossy(&output.stderr);
    let libs = notes
        .lines()
        .find_map(|l| l.split_once("native-static-libs:"))
        .map(|(_, libs)| {
            libs.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default_native_libs);
    let archive = workspace_target_dir()?
        .join("release")
        .join("libnoeta_aot.a");
    if !archive.exists() {
        return Err(format!(
            "the AOT runtime archive was not found at {} after building",
            archive.display()
        ));
    }
    Ok((archive, libs))
}

/// A conservative default native-link set for a Rust staticlib on Linux, used when the exact
/// `native-static-libs` note is unavailable (an explicit archive with no `NOETA_AOT_LINK_LIBS`).
#[cfg(feature = "jit")]
fn default_native_libs() -> Vec<String> {
    [
        "-lgcc_s",
        "-lutil",
        "-lrt",
        "-lpthread",
        "-lm",
        "-ldl",
        "-lc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The workspace's Cargo target directory: `CARGO_TARGET_DIR` if set, else `<workspace root>/target`
/// found by walking up from the current directory for the `Cargo.toml` that declares `[workspace]`.
#[cfg(feature = "jit")]
fn workspace_target_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        return Ok(std::path::PathBuf::from(dir));
    }
    let mut dir = std::env::current_dir().map_err(|e| e.to_string())?;
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists()
            && std::fs::read_to_string(&manifest)
                .map(|s| s.contains("[workspace]"))
                .unwrap_or(false)
        {
            return Ok(dir.join("target"));
        }
        if !dir.pop() {
            return Err(
                "cannot find the workspace root; run `noeta build --native` from inside the \
                 workspace, or set NOETA_AOT_RUNTIME_LIB to a prebuilt archive"
                    .to_string(),
            );
        }
    }
}

/// Link the AOT `object` against the runtime `archive` (+ its native `libs`) into `out` with a C
/// toolchain. Everything — the program's native bodies, the runtime, and Rust std — lives in the one
/// archive, so a single archive mention resolves the object↔runtime mutual references (the object
/// defines `noeta_aot_dispatch`, the archive defines `main` + the `noeta_jit_*` helpers). `cc` adds
/// the C runtime that calls `main`. The linker (`cc`) is overridable via `NOETA_CC`.
#[cfg(feature = "jit")]
fn link_native(
    object: &std::path::Path,
    archive: &std::path::Path,
    libs: &[String],
    out: &std::path::Path,
) -> Result<(), String> {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    let mut cmd = std::process::Command::new(&cc);
    cmd.arg(object).arg(archive).args(libs).arg("-o").arg(out);
    let output = cmd
        .output()
        .map_err(|e| format!("cannot run the linker `{cc}` (override with NOETA_CC): {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{cc}` exited with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Gate a tier runner on `--profile`: if a profile was given and does not make `tier` live, print a
/// note and return the success exit code (the runner no-ops); on a resolution failure, print it and
/// return the error code. `None` means "proceed" (no profile gate). The caller runs its body only
/// when this returns `None`.
fn profile_gate(entry: &std::path::Path, profile: &Option<String>, tier: &str) -> Option<ExitCode> {
    match tier_active_in_profile(entry, profile, tier) {
        Ok(true) => None,
        Ok(false) => {
            println!(
                "tier `{tier}` is not active in profile `{}`",
                profile.as_deref().unwrap_or_default()
            );
            Some(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("lang: {err}");
            Some(ExitCode::from(1))
        }
    }
}

/// The outcome of running one `@test` fn: whether it passed, the failure message (the first
/// diagnostic, typically the assertion/panic), and anything it wrote to stdout (shown on failure).
struct TestOutcome {
    name: String,
    passed: bool,
    message: Option<String>,
    stdout: String,
}

/// `noeta test <FILE>` — discover the program's `@test` blocks (object-model slice 6) and run each
/// as an isolated test. Tests run concurrently (one fresh isolate per test) and, by default, **all**
/// of them run even after a failure; `--fail-fast` stops at the first failure. A test fails when its
/// fn aborts — a false `assert`/`panic` (or any runtime error) — and passes when it returns normally.
/// The program's own top-level "main" effects are not run: `noeta test` runs the tests, not the file.
fn cmd_test(
    file: &std::path::Path,
    fail_fast: bool,
    jobs: Option<usize>,
    group: &Option<String>,
    profile: &Option<String>,
) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "test") {
        return code;
    }
    let linked = match noeta_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return ExitCode::from(1);
        }
    };

    // Activate the `test` tier: inline its `@test` blocks as ordinary top-level declarations and
    // collect the test fns. An unknown-tier block is an E0036 (a typo must not silently vanish).
    let activated = noeta_check::activate_tiers(&linked.program, &["test"]);
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check the activated program once, so a broken test is a compile error reported a single
    // time here rather than redundantly inside every per-test run.
    let checked = noeta_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    if activated.tests.is_empty() {
        println!("no tests found");
        return ExitCode::SUCCESS;
    }

    // The setup every test shares: the program's declarations (and top-level bindings/globals),
    // with its own "main" effect statements removed. Each test then runs as `setup + <call the test
    // fn>` in a fresh isolate, so the program's `echo`s don't run and one test cannot observe
    // another's state.
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    // The `--group` filter (object-model slice 6h): keep only tests tagged `#[Group("<g>")]`.
    let selected: Vec<&TierFn> = match group {
        Some(g) => activated
            .tests
            .iter()
            .filter(|t| test_group(t).as_deref() == Some(g.as_str()))
            .collect(),
        None => activated.tests.iter().collect(),
    };
    if selected.is_empty() {
        match group {
            Some(g) => println!("no tests in group `{g}`"),
            None => println!("no tests found"),
        }
        return ExitCode::SUCCESS;
    }

    // Partition into skipped (`#[Skip]`) and runnable. A skipped test is reported but never run, and
    // never fails the suite (a skipped `#[Data]` test counts as one skip, not one per row).
    let (skipped_refs, runnable): (Vec<&TierFn>, Vec<&TierFn>) =
        selected.into_iter().partition(|t| test_is_skipped(t));
    let skipped: Vec<String> = skipped_refs.iter().map(|t| skip_label(t)).collect();

    // Expand each runnable test into its case(s): a `#[Data([…])]` test runs once per row (reported
    // `name[row]`); an ordinary test is a single zero-arg case.
    let cases: Vec<TestCase> = runnable.iter().flat_map(|t| test_cases(t)).collect();
    let total = cases.len() + skipped.len();
    let run_count = cases.len();
    let jobs = jobs
        .filter(|n| *n > 0)
        .unwrap_or_else(default_jobs)
        .min(run_count.max(1));
    let skipped_note = if skipped.is_empty() {
        String::new()
    } else {
        format!(", {} skipped", skipped.len())
    };
    println!(
        "running {run_count} test{} on {jobs} thread{}{skipped_note}",
        plural(run_count),
        plural(jobs),
    );

    let outcomes = run_tests(&setup, &cases, activated.program.span, jobs, fail_fast);
    report(&outcomes, &skipped, total)
}

/// One runnable test invocation: which fn to call, the report label, and an optional argument (a
/// `#[Data]` row — `None` for an ordinary zero-arg test). A `#[Data([a, b])]` test expands to one
/// `TestCase` per row.
struct TestCase {
    /// The fn to invoke.
    fn_name: String,
    /// The report label (`#[Name]`/fn name, suffixed `[row]` for a data case).
    display: String,
    /// The argument to pass.
    arg: CaseArg,
    /// Where the fn is declared (for the synthesized call's span).
    span: Span,
}

/// A test case's argument: none (an ordinary zero-arg test), a `#[Data]` row value, or an invalid
/// row whose literal cannot become a runtime value (the case fails with this message).
enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Expand a runnable test into its cases: one zero-arg case normally, or one per row when the test
/// carries `#[Data([…])]`. A row literal that cannot be a runtime value (e.g. a bare type name)
/// becomes a case that fails with a clear message rather than being silently dropped.
fn test_cases(test: &TierFn) -> Vec<TestCase> {
    let base = test_display_name(test);
    let Some(rows) = data_rows(test) else {
        return vec![TestCase {
            fn_name: test.name.clone(),
            display: base,
            arg: CaseArg::None,
            span: test.span,
        }];
    };
    rows.iter()
        .map(|row| {
            let arg = match attr_value_to_expr(row, test.span) {
                Some(expr) => CaseArg::Value(expr),
                None => CaseArg::Invalid(format!(
                    "`#[Data]` row `{}` is not a runtime value",
                    attr_value_label(row)
                )),
            };
            TestCase {
                fn_name: test.name.clone(),
                display: format!("{base}[{}]", attr_value_label(row)),
                arg,
                span: test.span,
            }
        })
        .collect()
}

/// Convert a `#[Data]` row literal to an expression to pass as the test argument. Scalars and lists
/// (recursively) are supported; other literal forms (map/set/enum/struct/type-ref) return `None` and
/// surface as a failing case.
fn attr_value_to_expr(value: &AttrValue, span: Span) -> Option<Expr> {
    Some(match value {
        AttrValue::Str(s) => Expr::Str {
            value: s.clone(),
            span,
        },
        AttrValue::Int(n) => Expr::Int { value: *n, span },
        AttrValue::Float(f) => Expr::Float { value: *f, span },
        AttrValue::Bool(b) => Expr::Bool { value: *b, span },
        AttrValue::List(items) => Expr::List {
            items: items
                .iter()
                .map(|item| attr_value_to_expr(item, span))
                .collect::<Option<Vec<_>>>()?,
            span,
        },
        _ => return None,
    })
}

/// A short label for a `#[Data]` row, used in the `name[row]` case display.
fn attr_value_label(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(attr_value_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "?".to_string(),
    }
}

/// The rows of a `#[Data([…])]` attribute on `test`, if present — the elements of its list argument.
fn data_rows(test: &TierFn) -> Option<Vec<AttrValue>> {
    let attr = test
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TEST_ATTR_DATA)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::List(items) => Some(items.clone()),
        _ => None,
    })
}

/// Whether a test fn is marked `#[Skip]` — the runner reports it skipped and does not run it.
fn test_is_skipped(test: &TierFn) -> bool {
    test.attrs
        .iter()
        .any(|a| a.name == noeta_ast::reflect::TEST_ATTR_SKIP)
}

/// The report label for a skipped test: its display name, plus a `(reason)` when `#[Skip("reason")]`
/// gave one.
fn skip_label(test: &TierFn) -> String {
    let name = test_display_name(test);
    match string_attr(test, noeta_ast::reflect::TEST_ATTR_SKIP) {
        Some(reason) if !reason.is_empty() => format!("{name} ({reason})"),
        _ => name,
    }
}

/// A test's display name — the string in `#[Name("…")]` if present, else the fn's own name.
fn test_display_name(test: &TierFn) -> String {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_NAME).unwrap_or_else(|| test.name.clone())
}

/// A test's group — the string in `#[Group("…")]` if present, for `--group` filtering.
fn test_group(test: &TierFn) -> Option<String> {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_GROUP)
}

/// The first string-valued argument of the attribute named `name` on `test`, if any.
fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

/// Whether a top-level statement is tier-runner *setup* — a declaration or a global binding the
/// tests/benches may depend on — as opposed to the program's own "main" effects (which the
/// `noeta test`/`noeta bench` runners do not run; they run the tier fns, not the file).
fn is_tier_setup(stmt: &Stmt) -> bool {
    !matches!(
        stmt,
        Stmt::Echo { .. }
            | Stmt::Return { .. }
            | Stmt::If { .. }
            | Stmt::For { .. }
            | Stmt::While { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Expr { .. }
    )
}

/// A statement that calls fn `name` with `args`: `name(args…);`. Zero `args` is the ordinary
/// test/bench call; a single arg is a `#[Data]` row.
fn call_stmt(name: &str, args: Vec<Expr>, span: Span) -> Stmt {
    Stmt::Expr {
        expr: Expr::Call {
            callee: Box::new(Expr::Ident {
                name: name.to_string(),
                span,
            }),
            args,
            span,
        },
        span,
    }
}

/// The default test concurrency — the machine's available parallelism (1 if it cannot be queried).
fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Run `cases` concurrently across `jobs` worker threads, each grabbing the next case by an atomic
/// index. By default every case runs; with `fail_fast` a failure sets a shared stop flag and the
/// workers drain out. Results are gathered with their original index and returned in declaration
/// order, so the report is deterministic regardless of completion order.
fn run_tests(
    setup: &[Stmt],
    cases: &[TestCase],
    span: Span,
    jobs: usize,
    fail_fast: bool,
) -> Vec<TestOutcome> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results: Mutex<Vec<(usize, TestOutcome)>> = Mutex::new(Vec::with_capacity(cases.len()));

    thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    if fail_fast && stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= cases.len() {
                        break;
                    }
                    let outcome = run_one_test(setup, &cases[idx], span);
                    let failed = !outcome.passed;
                    results.lock().unwrap().push((idx, outcome));
                    if fail_fast && failed {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });

    let mut collected = results.into_inner().unwrap();
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, outcome)| outcome).collect()
}

/// Run a single test case: synthesize `setup + <call the fn (with its data arg, if any)>`, run it in
/// a fresh real-host isolate, and read a nonzero exit / any diagnostic as a failure (the first
/// diagnostic — the assertion or panic — is the reported message). An invalid `#[Data]` row fails
/// without running. The synthesized program is a subset of the already-checked activated program
/// plus one call, so it cannot introduce new type errors; one is surfaced as a failure rather than
/// panicking the worker.
fn run_one_test(setup: &[Stmt], case: &TestCase, span: Span) -> TestOutcome {
    let args = match &case.arg {
        CaseArg::None => Vec::new(),
        CaseArg::Value(expr) => vec![expr.clone()],
        CaseArg::Invalid(message) => {
            return TestOutcome {
                name: case.display.clone(),
                passed: false,
                message: Some(message.clone()),
                stdout: String::new(),
            };
        }
    };
    let display = case.display.clone();
    let mut stmts = setup.to_vec();
    stmts.push(call_stmt(&case.fn_name, args, case.span));
    let program = Program { stmts, span };

    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return TestOutcome {
            name: display,
            passed: false,
            message: Some(checked.diagnostics[0].message.clone()),
            stdout: String::new(),
        };
    }

    // `@test`/`@bench` compile a *separate* module per case (a different granularity than the
    // whole-file startup cache), so they don't participate in it — see `plans/startup-cache`.
    match execute_real_host(&program, &checked) {
        // The `@test` runner reports the failing diagnostic; the trace is a `noeta run` affordance.
        Ok((result, _trace)) => {
            let passed = result.exit_code == 0 && result.diagnostics.is_empty();
            let message = (!passed).then(|| {
                result
                    .diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| format!("exited with code {}", result.exit_code))
            });
            TestOutcome {
                name: display,
                passed,
                message,
                stdout: result.stdout,
            }
        }
        Err(err) => TestOutcome {
            name: display,
            passed: false,
            message: Some(err),
            stdout: String::new(),
        },
    }
}

/// Print the per-test report and the summary, returning the process exit code (success only when
/// every selected test ran and passed — a `#[Skip]`ped test does not fail the suite). Failing tests
/// show their message and any captured stdout; skipped tests are listed after. `total` counts every
/// selected test (run + skipped); `outcomes` are the runnable ones (fewer than run on `--fail-fast`).
fn report(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
    let mut passed = 0usize;
    for outcome in outcomes {
        if outcome.passed {
            passed += 1;
            println!("  ok    {}", outcome.name);
        } else {
            println!("  FAIL  {}", outcome.name);
            if let Some(message) = &outcome.message {
                println!("        {message}");
            }
            for line in outcome.stdout.lines() {
                println!("        | {line}");
            }
        }
    }
    for name in skipped {
        println!("  skip  {name}");
    }

    let failed = outcomes.len() - passed;
    let not_run = total - skipped.len() - outcomes.len();
    println!();
    let mut parts = vec![format!("{passed} passed"), format!("{failed} failed")];
    if !skipped.is_empty() {
        parts.push(format!("{} skipped", skipped.len()));
    }
    if not_run > 0 {
        parts.push(format!("{not_run} not run (stopped early)"));
    }
    parts.push(format!("{total} total"));
    println!("{}", parts.join(", "));
    let _ = io::stdout().flush();

    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The default number of iterations a benchmark runs when neither the `--iterations` flag nor a
/// per-bench `@bench(iterations: N)` directive sets one. Small, because the runner executes
/// *interpreted* code and measures at both N and 2N (see [`cmd_bench`]); a heavy body lowers it.
const DEFAULT_BENCH_ITERATIONS: u64 = 200;

/// `noeta bench <FILE>` — discover the program's `@bench` blocks (object-model slice 6) and measure
/// each. Unlike `noeta test`, benchmarks run **sequentially** (concurrency would corrupt timings).
/// Each bench's per-iteration cost is estimated by a **two-point** measurement: the fn is invoked N
/// and 2N times in fresh isolates and the per-iteration time is `(t(2N) − t(N)) / N`, which cancels
/// the fixed per-run overhead (runtime startup, global/setup evaluation, IR lowering — all identical
/// between the two runs). N comes from `--iterations`, else the per-bench `@bench(iterations: N)`
/// directive, else [`DEFAULT_BENCH_ITERATIONS`].
fn cmd_bench(
    file: &std::path::Path,
    iterations_override: Option<u64>,
    profile: &Option<String>,
) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "bench") {
        return code;
    }
    let linked = match noeta_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return ExitCode::from(1);
        }
    };

    // Activate the `bench` tier: inline its `@bench` blocks as ordinary top-level declarations and
    // collect the bench fns (with their directive args). An unknown-tier block is an E0036.
    let activated = noeta_check::activate_tiers(&linked.program, &["bench"]);
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check once, so a broken benchmark is a compile error reported here rather than inside
    // every per-bench run.
    let checked = noeta_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    if activated.benches.is_empty() {
        println!("no benchmarks found");
        return ExitCode::SUCCESS;
    }

    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    let total = activated.benches.len();
    println!("running {total} benchmark{}", plural(total));

    let mut failed = 0usize;
    for bench in &activated.benches {
        let n = iterations_override
            .or_else(|| iterations_arg(&bench.args))
            .unwrap_or(DEFAULT_BENCH_ITERATIONS)
            .max(1);
        match (
            measure_iterations(&setup, bench, n),
            measure_iterations(&setup, bench, n.saturating_mul(2)),
        ) {
            (Ok(t1), Ok(t2)) => {
                let per_ns = ((t2.as_nanos() as f64 - t1.as_nanos() as f64) / n as f64).max(0.0);
                println!(
                    "  {:<28} {:>11}/iter  ({n} iterations)",
                    bench.name,
                    fmt_per_iter(per_ns),
                );
            }
            (Err(msg), _) | (_, Err(msg)) => {
                failed += 1;
                println!("  {:<28} FAILED: {msg}", bench.name);
            }
        }
    }

    println!();
    println!("{} ran, {failed} failed, {total} total", total - failed,);
    let _ = io::stdout().flush();
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The `iterations` argument of a `@bench(...)` directive, if present and positive — the per-bench
/// override of the default iteration count. Resolved through the shared tier-arg schema, so both the
/// positional (`@bench(1000)`) and named (`@bench(iterations: 1000)`) forms work identically.
fn iterations_arg(args: &[AttrArg]) -> Option<u64> {
    match noeta_check::bind_tier_args("bench", args)
        .values
        .get("iterations")
    {
        Some(AttrValue::Int(n)) if *n > 0 => Some(*n as u64),
        _ => None,
    }
}

/// Measure executing `bench` `n` times: synthesize `setup + n×<call the bench fn>`, then run it in a
/// fresh real-host isolate, timing **only execution** (IR lowering is done untimed, before the
/// clock starts). A discarded warm-up run plus the minimum of three measured runs damps noise. A
/// nonzero exit / any diagnostic (a panic in the bench body) is a failure, surfaced as `Err`.
fn measure_iterations(
    setup: &[Stmt],
    bench: &TierFn,
    n: u64,
) -> Result<std::time::Duration, String> {
    let mut stmts = setup.to_vec();
    let call = call_stmt(&bench.name, Vec::new(), bench.span);
    stmts.reserve(n as usize);
    for _ in 0..n {
        stmts.push(call.clone());
    }
    let program = Program {
        stmts,
        span: bench.span,
    };

    let checked = noeta_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return Err(checked.diagnostics[0].message.clone());
    }

    // Take the minimum of three runs: `min` is the standard robust estimator (the fastest run is
    // the one least perturbed by scheduler/GC/OS noise) and inherently discards the cold first run,
    // so no separate warm-up is needed.
    let mut best: Option<std::time::Duration> = None;
    for _ in 0..3 {
        let (result, elapsed) = bench_execute(&program, &checked)?;
        if result.exit_code != 0 || !result.diagnostics.is_empty() {
            return Err(result
                .diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| format!("exited with code {}", result.exit_code)));
        }
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    Ok(best.expect("three measured runs"))
}

/// Lower a program for the real host (untimed) and execute it, returning the result and the
/// **execution-only** wall-clock duration (lowering excluded). Mirrors [`execute_real_host`]'s
/// pipeline so a benchmark runs the same Core-IR path a normal `noeta run` does.
fn bench_execute(
    program: &Program,
    checked: &noeta_check::Checked,
) -> Result<(noeta_backend::RunResult, std::time::Duration), String> {
    let host =
        noeta_runtime::RealHost::new().map_err(|err| format!("cannot start the runtime: {err}"))?;
    // Compile to bytecode untimed (isolates I.4a — the real path is the VM), then time execution
    // alone, so the measurement excludes both lowering and bytecode generation.
    let module = compile_real(program, checked)?;
    let start = Instant::now();
    let result = VmBackend::new().run_module_with_host(&module, Box::new(host));
    Ok((result, start.elapsed()))
}

/// Format a per-iteration duration (in nanoseconds) with an adaptive unit, so a fast op reads in
/// `ns` and a slow one in `ms`/`s`.
fn fmt_per_iter(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// `noeta doc <FILE>` — extract the program's `@doc { … }` text blocks (object-model slice 6f) to
/// stdout, in source order. Each block's verbatim body is dedented (the common leading indentation
/// and the surrounding blank lines from sitting inside `@doc { … }` are stripped) and preceded by an
/// HTML-comment header noting its source location — valid markdown that renders to nothing. The
/// program is not type-checked or run; doc extraction works on a parse alone, so docs can be pulled
/// from work-in-progress code.
fn cmd_doc(file: &std::path::Path, profile: &Option<String>) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "doc") {
        return code;
    }
    let linked = match noeta_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return ExitCode::from(1);
        }
    };

    let docs = noeta_check::collect_docs(&linked.program);
    if docs.is_empty() {
        eprintln!("lang: no `@doc` blocks found");
        return ExitCode::SUCCESS;
    }

    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let source = linked.sources.source(doc.span.source);
        let line = source.line_col(doc.span.start).line;
        out.push_str(&format!("<!-- {}:{} -->\n", source.name(), line));
        out.push_str(&dedent(&doc.text));
        out.push('\n');
    }
    print!("{out}");
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

/// Dedent a verbatim doc body for presentation: drop leading/trailing blank lines, then strip the
/// common leading whitespace shared by all non-blank lines (so text written indented inside
/// `@doc { … }` renders flush-left). Blank lines do not count toward the common indent and are
/// emitted empty. The lexer captured the body exactly; this is purely the doc generator's
/// formatting, leaving the AST's bytes untouched.
fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // Trim leading and trailing blank lines.
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(start);
    let body = &lines[start..=end];

    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    body.iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether an entry was consumed (evaluated or reported) or is still incomplete and needs
/// more input (multiline continuation).
enum ReplStep {
    Consumed,
    Incomplete,
}

/// The environment the REPL runs against: a **real** host + wall-clock executor, so `fs`, `time`,
/// `env`, and `uuid()` work at the prompt against the real machine — exactly as `noeta run` does. Built
/// fresh on `:reset`. (The deterministic sandbox exists only to make the differential oracle
/// reproducible; it is not what an interactive prompt wants.) A real-thread `isolate f(args)` falls
/// back to a cooperative task here, since the session does not arm the parallel-isolate path.
fn real_repl_env() -> noeta_vm::HostFactory {
    Box::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_runtime::RealHost::new().expect("cannot start the REPL's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_runtime::RealExecutor::new().expect("cannot start the REPL's async executor"),
        );
        (host, executor)
    })
}

/// Load, check, compile, and RUN the `--load` bootstrap, returning the adopted session and the
/// bootstrap's sources (the REPL's entry ids continue after them). The bootstrap is a *file*, so
/// it is always fully checked — as it would be under `noeta run` — regardless of `--no-check`
/// (which governs prompt entries); with checking on, the bootstrap's own checker session carries
/// forward, so entries check against everything it declared and bound. Isolates in a bootstrap run
/// cooperatively (the session's execution model). Any failure — unreadable file, load/check
/// diagnostics, a runtime abort — exits with diagnostics instead of opening a broken prompt.
fn repl_bootstrap(
    path: &std::path::Path,
    checker: &mut Option<noeta_check::SessionChecker>,
) -> Result<(VmSession, Vec<Source>), ExitCode> {
    let linked = match noeta_loader::load(path) {
        Err(err) => {
            eprintln!("noeta: cannot read {}: {err}", path.display());
            return Err(ExitCode::from(2));
        }
        Ok(Err(load_diagnostics)) => {
            for ld in &load_diagnostics {
                eprint!("{}", render(&ld.source, &ld.diagnostic));
            }
            return Err(ExitCode::FAILURE);
        }
        Ok(Ok(linked)) => linked,
    };

    // Always checked (it is a file); the session flavor keeps the checker when the prompt wants it.
    let (checked, session_checker) = noeta_check::check_all_session(&linked.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return Err(ExitCode::FAILURE);
    }
    if checker.is_some() {
        *checker = Some(session_checker);
    }

    // Cooperative isolates + no debug info: the prompt's own execution model.
    let (module, compiler) = match noeta_compiler::compile_with_sites_session(
        &linked.program,
        checked.sites,
        false,
        false,
    ) {
        Ok(pair) => pair,
        Err(u) => {
            eprintln!(
                "noeta: internal error: the VM cannot compile this program: {}",
                u.reason
            );
            return Err(ExitCode::FAILURE);
        }
    };
    let (session, out) = VmSession::adopted(&module, compiler, real_repl_env());
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    if !out.diagnostics.is_empty() {
        // A bootstrap that aborts is a broken app context — fail fast, exactly like `noeta run`.
        emit_diagnostics_mapped(&linked.sources, out.diagnostics.iter());
        emit_trace(&out.trace, &linked.sources);
        return Err(ExitCode::FAILURE);
    }
    eprintln!("(loaded {})", path.display());
    Ok((session, linked.sources.into_sources()))
}

fn cmd_repl(check: bool, load: Option<PathBuf>) -> ExitCode {
    let stdin = io::stdin();
    // The optional per-entry type checker (session-checker C2): `Some` = every entry is checked
    // against the accumulated session before it runs; an erroring entry prints diagnostics and is
    // skipped (and commits nothing — `check_entry` is transactional). Toggleable at the prompt.
    // A `--load` bootstrap replaces the fresh checker with one seeded from the bootstrap's own
    // whole-program check, so entries check against everything the bootstrap declared.
    let mut checker: Option<noeta_check::SessionChecker> =
        check.then(noeta_check::SessionChecker::new);
    // A bootstrapped session ("tinker"): the file runs to completion as entry 0 — checked,
    // imports resolved — and the prompt opens over its final state. Its sources seed the entry
    // list so later `SourceId`s continue past them (a trace into a bootstrap function renders
    // against its real text).
    let (mut session, preloaded_sources) = match &load {
        None => (VmSession::new(real_repl_env()), Vec::new()),
        Some(path) => match repl_bootstrap(path, &mut checker) {
            Ok(booted) => booted,
            Err(code) => return code,
        },
    };
    // Whether SITE-DRIVEN codegen is still sound (session-checker C5): true only while the checker
    // has seen every entry of the session. `:check off` clears it PERMANENTLY — precise destructor
    // relevance derived from a registry that missed an unchecked entry's `destruct` class could
    // skip a destructor, so once any entry runs unchecked the session stays on conservative
    // codegen even if checking is turned back on (diagnostics return; the codegen upgrade doesn't).
    let mut precise_codegen = check;
    let mut buffer = String::new();
    // Each evaluated entry is parsed with a **distinct** `SourceId` (its index here) and kept, so a
    // stack trace into a function defined in an *earlier* entry renders against that entry's real text
    // and line — rather than degrading to name-only, as it did when every entry reused
    // `SourceId::FIRST` (REPL-on-VM follow-on). Only entries that actually run are kept; a syntax-error
    // entry compiles nothing, so no future trace can reference it.
    let mut sources: Vec<Source> = preloaded_sources;
    eprint!("lang repl — type a statement, Ctrl-D to exit\n» ");
    let _ = io::stderr().flush();

    eprintln!("type :help for commands");
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Skip blank lines when nothing is pending.
        if buffer.is_empty() && line.trim().is_empty() {
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        // A `:`-prefixed line (when nothing is pending) is a REPL meta-command — tooling that lives
        // outside the language grammar (`:type`, `:drop`, `:bindings`, `:reset`, `:help`, `:quit`).
        if buffer.is_empty() && line.trim_start().starts_with(':') {
            if repl_meta(
                &mut session,
                &mut checker,
                &mut precise_codegen,
                line.trim(),
                &sources,
            ) == MetaOutcome::Quit
            {
                break;
            }
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        match repl_step(
            &mut session,
            &mut checker,
            precise_codegen,
            &buffer,
            &mut sources,
        ) {
            ReplStep::Consumed => {
                buffer.clear();
                eprint!("» ");
            }
            // Keep the buffer and read another line; show a continuation prompt.
            ReplStep::Incomplete => eprint!("… "),
        }
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
}

/// Whether a meta-command asked to leave the REPL.
#[derive(PartialEq)]
enum MetaOutcome {
    Continue,
    Quit,
}

/// Handle a `:`-prefixed REPL meta-command. These are REPL *tooling*, deliberately outside the
/// language grammar (the language itself has no manual `drop`/`type` keyword): the REPL keeps
/// top-level bindings alive across entries — extended lifetime, unlike compiled code's last-use
/// destruction — so `:drop` is how a destructor is observed or an object reclaimed interactively,
/// and `:type` reports a value's runtime type in a session that runs no checker.
fn repl_meta(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: &mut bool,
    line: &str,
    sources: &[Source],
) -> MetaOutcome {
    let body = line.strip_prefix(':').unwrap_or(line);
    let mut parts = body.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "quit" | "q" => return MetaOutcome::Quit,
        "help" | "h" | "?" => print_repl_help(),
        "reset" => {
            session.reset();
            // The checker's session must reset with the runtime's — its registries describe
            // bindings/types that no longer exist. A reset session with checking on is fully
            // checked again, so precise codegen is earned back.
            if checker.is_some() {
                *checker = Some(noeta_check::SessionChecker::new());
                *precise_codegen = true;
            }
            eprintln!("(session reset)");
        }
        // Toggle per-entry type checking (session-checker C2). Turning it on mid-session starts
        // from an EMPTY typing environment: earlier (unchecked) bindings are simply unknown to it,
        // which the checker's unknown-ident tolerance treats as runtime-deferred — degraded
        // precision, never false errors.
        "check" => match arg {
            "on" => {
                if checker.is_none() {
                    *checker = Some(noeta_check::SessionChecker::new());
                }
                // Diagnostics return; the codegen upgrade does not — see `precise_codegen`.
                eprintln!("(type checking on — entries are checked before running)");
            }
            "off" => {
                *checker = None;
                *precise_codegen = false;
                eprintln!("(type checking off)");
            }
            _ => eprintln!(
                "type checking is {} — usage: :check on|off",
                if checker.is_some() { "on" } else { "off" }
            ),
        },
        "bindings" | "b" => {
            let names = session.binding_names();
            if names.is_empty() {
                eprintln!("(no bindings)");
            } else {
                println!("{}", names.join(", "));
                let _ = io::stdout().flush();
            }
        }
        "drop" | "free" => {
            if arg.is_empty() {
                eprintln!("usage: :drop <name>");
            } else {
                let (found, out) = session.drop_binding(arg);
                print!("{}", out.stdout);
                let _ = io::stdout().flush();
                if found {
                    eprintln!("(dropped `{arg}`)");
                } else {
                    eprintln!("no binding named `{arg}`");
                }
            }
        }
        "type" | "t" => {
            if arg.is_empty() {
                eprintln!("usage: :type <expr>");
            } else {
                repl_type(session, arg, sources);
            }
        }
        other => eprintln!("unknown command `:{other}` — try :help"),
    }
    MetaOutcome::Continue
}

/// `:type <expr>` — parse `expr`, evaluate it in the session, and print its runtime type. Evaluating
/// the expression may abort (`:type boom()`); the trace then resolves against every entry's source, so
/// a `:type` id (the next index) is added to the render map without being *persisted* — a `:type`
/// query defines nothing, so no later trace can reference it.
fn repl_type(session: &mut VmSession, expr: &str, sources: &[Source]) {
    let id = SourceId(sources.len() as u32);
    let fragment = parse_fragment(id, "<repl-type>", expr);
    if !fragment.diagnostics.is_empty() {
        emit_diagnostics(&fragment.source, fragment.diagnostics.iter());
        return;
    }
    let out = session.type_of(&fragment.program);
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    // Render diagnostics / any abort trace against all entries plus this `:type` source.
    let mut map_sources = sources.to_vec();
    map_sources.push(fragment.source);
    let map = SourceMap::new(map_sources);
    if !out.diagnostics.is_empty() {
        emit_diagnostics_mapped(&map, out.diagnostics.iter());
        emit_trace(&out.trace, &map);
    } else if let Some(ty) = out.value {
        println!("{ty}");
        let _ = io::stdout().flush();
    }
}

fn print_repl_help() {
    eprintln!("REPL commands:");
    eprintln!("  :type <expr>   show the runtime type of an expression (evaluates it)");
    eprintln!("  :drop <name>   run a binding's destructor now and unbind it (alias :free)");
    eprintln!("  :bindings      list the live bindings");
    eprintln!("  :reset         clear all bindings and start fresh");
    eprintln!("  :check on|off  type-check entries before running them (skip on error)");
    eprintln!("  :help          show this help");
    eprintln!("  :quit          exit the REPL (or Ctrl-D)");
}

/// Evaluate one checked-clean entry: with the checker on AND every prior entry checked
/// (`precise_codegen`), the entry compiles with the checker's accumulated site bundle — the same
/// site-driven codegen the file pipeline runs (session-checker C5); otherwise the conservative
/// checkerless codegen, which is always sound.
fn eval_entry(
    session: &mut VmSession,
    checker: &Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    program: &noeta_ast::Program,
) -> noeta_vm::SessionOutput {
    match checker {
        Some(checker) if precise_codegen => {
            session.eval_checked(program, &checker.sites_snapshot())
        }
        _ => session.eval(program),
    }
}

/// The `--check` gate (session-checker C2): type-check one parsed entry against the accumulated
/// session. Returns whether the entry should RUN — `true` with no checker, or when the entry has
/// no error-severity diagnostics (warnings print and the entry still runs). Errors render against
/// the entry's own source and the entry is skipped; `check_entry`'s transactionality means a
/// skipped entry left no trace in the checker either.
fn check_entry_gate(
    checker: &mut Option<noeta_check::SessionChecker>,
    program: &noeta_ast::Program,
    source: &Source,
) -> bool {
    let Some(checker) = checker.as_mut() else {
        return true;
    };
    let diagnostics = checker.check_entry(program);
    if diagnostics.is_empty() {
        return true;
    }
    emit_diagnostics(source, diagnostics.iter());
    !diagnostics
        .iter()
        .any(|d| d.severity == noeta_diagnostics::Severity::Error)
}

/// Try to evaluate the accumulated REPL buffer. Statements ending in `;`/`}` evaluate as-is;
/// a bare expression (no trailing `;`) is retried with a `;` appended so its value can be
/// printed. If the only parse problem is hitting end-of-input, the entry is treated as
/// incomplete and more input is requested (multiline). Any other error is reported, and the
/// buffer is reset so one bad entry cannot wedge the session.
///
/// With a `checker` present (`--check` / `:check on`, session-checker C2), a parsed entry is
/// type-checked against the accumulated session first: an entry with errors prints its `E0xxx`
/// diagnostics (rendered against the entry's own source) and is **skipped** — and `check_entry`'s
/// transactionality means it commits nothing, so the checker stays aligned with what actually ran.
/// Warning-only entries print the warnings and run.
fn repl_step(
    session: &mut VmSession,
    checker: &mut Option<noeta_check::SessionChecker>,
    precise_codegen: bool,
    buffer: &str,
    sources: &mut Vec<Source>,
) -> ReplStep {
    // The next evaluated entry's `SourceId` is its index in the persistent `sources` vector.
    let id = SourceId(sources.len() as u32);
    let source = Source::new(id, format!("<repl:{}>", sources.len()), buffer.to_string());
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let diags: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .collect();

    if diags.is_empty() {
        if !check_entry_gate(checker, &parsed.program, &source) {
            return ReplStep::Consumed;
        }
        sources.push(source);
        let out = eval_entry(session, checker, precise_codegen, &parsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // A bare expression needs a terminating `;`; retry with one appended (same id — only one of the
    // two sources is ever kept, whichever compiled).
    let psource = Source::new(
        id,
        format!("<repl:{}>", sources.len()),
        format!("{buffer};"),
    );
    let plexed = lex(&psource);
    let pparsed = parse(&psource, &plexed.tokens);
    if plexed.diagnostics.is_empty() && pparsed.diagnostics.is_empty() {
        if !check_entry_gate(checker, &pparsed.program, &psource) {
            return ReplStep::Consumed;
        }
        sources.push(psource);
        let out = eval_entry(session, checker, precise_codegen, &pparsed.program);
        emit_session(sources, out);
        return ReplStep::Consumed;
    }

    // An entry with unclosed `(`/`{`/`[` is a multi-line definition still being typed (a `class`,
    // a `fn` body, a multi-line list/object literal). The parser may report a *non*-end-of-input
    // error inside such a buffer rather than cleanly running out of tokens, so the end-of-input
    // check below is not enough on its own — gather more lines until the delimiters balance. The
    // count is over lexer tokens, so braces inside string/template literals (a single token) and
    // `${…}` interpolation never miscount.
    if unclosed_delimiters(&lexed.tokens) {
        return ReplStep::Incomplete;
    }

    // Only end-of-input errors → the entry is unfinished; gather more lines.
    if diags
        .iter()
        .all(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
    {
        return ReplStep::Incomplete;
    }

    // A genuine syntax error: report it against the original buffer and reset. The entry compiled
    // nothing, so its source is *not* kept — its id is reused by the next entry.
    emit_diagnostics(&source, diags.iter());
    ReplStep::Consumed
}

/// Whether `tokens` has more opening than closing delimiters — i.e. a `(`/`{`/`[` left unclosed, the
/// signature of a multi-line REPL entry still being typed. A single net depth across all three kinds
/// is enough to decide *incompleteness* (the parser validates correct nesting once the buffer is
/// balanced); a buffer that closes more than it opens (net ≤ 0) is left to the parser to report.
fn unclosed_delimiters(tokens: &[noeta_lexer::Token]) -> bool {
    let mut depth: i32 = 0;
    for token in tokens {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// Print a session evaluation's stdout, the value of a trailing bare expression (if any), then any
/// diagnostics and abort trace. `sources` holds every evaluated entry keyed by `SourceId`, so a
/// diagnostic or trace frame from a function defined in an earlier entry renders against that entry's
/// real file and line.
fn emit_session(sources: &[Source], out: SessionOutput) {
    print!("{}", out.stdout);
    if let Some(value) = out.value {
        println!("{value}");
    }
    let _ = io::stdout().flush();
    let map = SourceMap::new(sources.to_vec());
    emit_diagnostics_mapped(&map, out.diagnostics.iter());
    emit_trace(&out.trace, &map);
}

/// Render a session entry's abort trace to stderr, resolving each frame against `map` — the same
/// rendering and "only when there is a real call chain (≥2 frames)" rule `noeta run` uses (a single
/// frame just repeats the diagnostic's own location). With per-entry sources in `map`, a frame from a
/// function defined in an earlier entry now shows that entry's real file and line.
fn emit_trace(trace: &[noeta_vm::TraceFrame], map: &SourceMap) {
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(trace, map));
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<noeta_lexer::Token> {
        lex(&Source::new(SourceId::FIRST, "<t>", src.to_string())).tokens
    }

    #[test]
    fn unclosed_delimiters_detects_in_progress_multiline_entries() {
        // Open `{`/`(`/`[` with no match → still being typed (a `class`, `fn` body, or literal).
        assert!(unclosed_delimiters(&toks("class Res {")));
        assert!(unclosed_delimiters(&toks(
            "fn run(): void {\n  mut r = Res.new(3);"
        )));
        assert!(unclosed_delimiters(&toks("[1,\n 2,")));
        assert!(unclosed_delimiters(&toks("f(")));
        // Balanced (or over-closed) → let the parser decide, not "incomplete".
        assert!(!unclosed_delimiters(&toks("class Res { id: int }")));
        assert!(!unclosed_delimiters(&toks("[1, 2, 3]")));
        assert!(!unclosed_delimiters(&toks("x = 5;")));
        assert!(!unclosed_delimiters(&toks("}")));
        // Braces inside a template string are one token — they never miscount.
        assert!(!unclosed_delimiters(&toks("echo \"drop ${id}\";")));
    }
}
