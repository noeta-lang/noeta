//! The lean production runtime's shared execution core (dev-deps D3).
//!
//! This crate is the app-execution engine — VM + real [`Host`](noeta_stdlib::Host) + runtime
//! extensions (L1), and in later slices the compile front-end (L2) — with **no dev tooling (L3)**:
//! no fmt, no formatter/parser (`malva`), no LSP/DAP/MCP. `noeta-cli` depends on it so the CLI's
//! `run`/`build --exe` path and the standalone `noeta-runner` binary share ONE execution core — the
//! drift firewall (see `plans/dev-deps`). The toolchain is excluded *structurally*: those crates are
//! simply not dependencies here, so a shipped artifact built on this crate cannot reach them.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use noeta_backend::RunResult;
use noeta_bytecode::Module;
use noeta_diagnostics::render_mapped;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::{JitReport, TraceFrame, VmBackend};

pub mod compile;

pub use compile::{Compiled, CompileFailure, compile_real, compile_whole_file, resolve_providers};

/// Run an already-compiled [`Module`] against the real host — the shared execution core of the
/// CLI's source-run path and the `.noeb` bundle runner (P-AOT L1.2), which loads a module directly
/// with no source to compile.
///
/// `app_id` is the p2p application namespace (p2p P3.4), computed by the caller and passed in — the
/// CLI derives it from the nearest `noeta.toml`; the standalone runner passes `None` to let the
/// runtime fall back to the executable's file stem. It is a parameter (not computed here) precisely
/// so this crate never links the package manager: keeping the registry/keyless surface out of the
/// lean runtime is the whole point.
///
/// A factory mints a fresh real host + wall-clock executor per isolate (isolates I.4b): the main
/// program gets one, and each real-thread `isolate f(args)` gets its own so a worker's disk / clock
/// / async state is independent. Injected here (not in `noeta-vm`) so the VM crate needs no
/// `noeta-runtime`/tokio dependency.
pub fn run_module_real_host(
    module: Arc<Module>,
    args: Vec<String>,
    app_id: Option<String>,
    jit_report: bool,
) -> (RunResult, Vec<TraceFrame>, Option<JitReport>) {
    let factory: noeta_vm::IsolateFactory = Arc::new(move || {
        let host: Box<dyn noeta_stdlib::Host> = Box::new(
            noeta_runtime::RealHost::new()
                .expect("cannot start an isolate's runtime")
                .with_args(args.clone())
                .with_p2p_app(app_id.clone()),
        );
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_runtime::RealExecutor::new().expect("cannot start an isolate's async executor"),
        );
        (host, executor)
    });
    let (host, executor) = factory();
    // Real isolates run on OS threads (out-of-oracle); channel-shipping isolates fall back to
    // cooperative tasks (cross-thread channels are I.4c). The differential keeps the sandbox pair.
    VmBackend::new()
        .run_module_with_host_and_executor_parallel(module, host, executor, factory, jit_report)
}

/// Run a compiled module and render its stdout / diagnostics / abort trace / `--jit-stats` report to
/// the process streams, returning the exit code. Shared by the CLI (`run`, bundle run) and the
/// standalone runner so both present identical output. `app_id` threads through to
/// [`run_module_real_host`]; `jit_stats` requests the tier-1 report (a JIT-less binary says so
/// rather than print nothing).
pub fn run_compiled_module(
    module: Arc<Module>,
    sources: &SourceMap,
    args: Vec<String>,
    app_id: Option<String>,
    jit_stats: bool,
) -> ExitCode {
    let (result, trace, report) = run_module_real_host(Arc::clone(&module), args, app_id, jit_stats);
    print!("{}", result.stdout);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(render_mapped(sources, result.diagnostics.iter()).as_bytes());
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, sources));
    }
    // `--jit-stats`: the report renders after the program's own output, to stderr (it is
    // diagnostics, not program output). A JIT-less binary produces no report — say so rather than
    // print nothing.
    match report {
        Some(report) => eprint!("{}", render_jit_report(&report, &module, sources)),
        None if jit_stats => {
            eprintln!("lang: --jit-stats: this binary was built without the JIT (no report)");
        }
        None => {}
    }
    exit_code(result.exit_code)
}

/// The process exit code for a program result, clamping out-of-`u8` codes to 1.
pub fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Decode a `.noeb` bundle blob and run it (P-AOT L1.2) — no source, no compile. `file` labels the
/// synthetic source that diagnostics render against; `args` is the program's argument vector
/// (`args.all()`), `app_id` its p2p namespace. Shared by the CLI's bundle path and the standalone
/// runner's stapled/two-file paths.
pub fn run_bundle_bytes(
    file: &Path,
    bytes: &[u8],
    args: Vec<String>,
    app_id: Option<String>,
    jit_stats: bool,
) -> ExitCode {
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
    run_compiled_module(Arc::new(module), &sources, args, app_id, jit_stats)
}

/// Compile a `.noe` **source** file (the PHP-style deploy) and run it — the same L2 pipeline the CLI
/// uses (`compile_whole_file`), then the shared L1 execution tail. `tiers`/`target`/`no_cache` steer
/// tier activation and the startup cache; `args`/`app_id` reach the program as in [`run_bundle_bytes`].
/// A compile failure is rendered and its exit code returned.
pub fn run_source_file(
    file: &Path,
    tiers: &[String],
    target: &Option<String>,
    no_cache: bool,
    args: Vec<String>,
    app_id: Option<String>,
    jit_stats: bool,
) -> ExitCode {
    match compile::compile_whole_file(file, tiers, target, no_cache) {
        Ok(compiled) => run_compiled_module(compiled.module, &compiled.sources, args, app_id, jit_stats),
        Err(failure) => failure.report(),
    }
}

/// P-AOT L2: detect and run a bundle stapled onto *this* executable (a `noeta build --exe`
/// artifact), returning its exit code — or `None` when there is no trailer (the plain toolchain
/// binary, or a bare runner), so the caller runs its normal path. Reads only the fixed trailer +
/// embedded blob, seeking rather than slurping the whole binary, so a no-bundle startup is not
/// taxed. Any IO/format hiccup is treated as "no bundle".
///
/// `resolve_app_id` computes the p2p namespace from the program's argv (which this reads): the CLI
/// passes its `noeta.toml`-aware resolver, the lean runner passes `|_| None` (executable file-stem
/// default). A shipped artifact is invoked directly, so its real process argv *is* the program's
/// argument vector — passed straight through to `args.all()`.
pub fn try_run_stapled(
    resolve_app_id: impl FnOnce(&[String]) -> Option<String>,
) -> Option<ExitCode> {
    use std::io::{Read, Seek, SeekFrom};

    let exe_path = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe_path).ok()?;
    let size = file.seek(SeekFrom::End(0)).ok()?;
    let trailer_len = noeta_bundle::TRAILER_LEN as u64;
    if size < trailer_len {
        return None;
    }
    file.seek(SeekFrom::End(-(trailer_len as i64))).ok()?;
    let mut trailer = [0u8; noeta_bundle::TRAILER_LEN];
    file.read_exact(&mut trailer).ok()?;
    let blob_len = noeta_bundle::stapled_len(&trailer)?;
    let blob_start = size.checked_sub(trailer_len + blob_len as u64)?;
    file.seek(SeekFrom::Start(blob_start)).ok()?;
    let mut blob = vec![0u8; blob_len];
    file.read_exact(&mut blob).ok()?;
    let argv: Vec<String> = std::env::args().collect();
    let app_id = resolve_app_id(&argv);
    Some(run_bundle_bytes(&exe_path, &blob, argv, app_id, false))
}

/// Render the `--jit-stats` report (`noeta run --jit-stats`, stderr): tier-1 compile coverage, the
/// bail histogram, and OSR-declined loops — each site resolved to its function, source line, and
/// disassembled op. The histogram counts **native entries** that fell back (a frame stays tier-0
/// after a bail until its next entry), so read it as "how often the fallback happened", not a
/// per-iteration figure; a declined loop never enters native at all, which is why it is its own
/// section.
fn render_jit_report(report: &JitReport, module: &Module, sources: &SourceMap) -> String {
    use std::fmt::Write as _;
    // "app.noe:12" for an op site, resolved through the line table (always emitted, even in
    // production compiles); "?" for a site before the first line entry (a prototype's prologue).
    let site = |proto: u32, pc: u32| -> String {
        module.protos[proto as usize]
            .line_span(pc as usize)
            .map(|span| {
                let lc = sources.line_col(span);
                format!("{}:{}", sources.source(span.source).name(), lc.line)
            })
            .unwrap_or_else(|| "?".to_string())
    };
    let fn_name = |proto: u32| -> String {
        module.protos[proto as usize]
            .name
            .clone()
            .unwrap_or_else(|| format!("proto {proto}"))
    };
    let op_text = |proto: u32, pc: u32| -> String {
        module.protos[proto as usize].op_repr_at(pc as usize, &module.names, &module.global_names)
    };

    let mut out = String::new();
    let stubs = report.compiled.saturating_sub(report.native);
    let _ = writeln!(
        out,
        "── JIT report ──\ntier 1: {} of {} compiled prototypes native ({} bail stubs), compile time {:.1} ms",
        report.native,
        report.compiled,
        stubs,
        report.compile_ns_total as f64 / 1e6,
    );

    if report.bails.is_empty() {
        let _ = writeln!(
            out,
            "\nno bail events — native code never fell back mid-frame"
        );
    } else {
        let _ = writeln!(
            out,
            "\nbail sites (native code fell back to the interpreter; counts are native entries, not iterations):"
        );
        for b in &report.bails {
            let _ = writeln!(
                out,
                "  {:>8}\u{00d7}  {}  {}  {}",
                b.count,
                site(b.proto, b.pc),
                fn_name(b.proto),
                op_text(b.proto, b.pc),
            );
        }
        if report.bails.iter().any(|b| b.pc == 0) {
            let _ = writeln!(
                out,
                "  (a pc-0 site can also be the entry parameter guard: a heap argument bails before the first op)"
            );
        }
    }

    if !report.declined.is_empty() {
        let _ = writeln!(
            out,
            "\nloops declined tier 1 (every loop contains a non-native op; the prototype ran interpreted):"
        );
        for d in &report.declined {
            let _ = writeln!(out, "  {} — blocked by:", fn_name(d.proto));
            for &pc in &d.bail_pcs {
                let _ = writeln!(out, "    {}  {}", site(d.proto, pc), op_text(d.proto, pc),);
            }
        }
    }
    out
}
