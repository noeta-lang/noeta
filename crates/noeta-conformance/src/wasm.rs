//! The wasm differential oracle (P-WASM W1.3): every corpus program, compiled to a `.noeb` and
//! executed by the **wasm runner under wasmtime** (`--sandbox`), must be byte-identical to the
//! native VM run — stdout, exit code, and rendered stderr (diagnostics + traceback).
//!
//! This is the ship-safety gate for the wasm target, the exact analogue of the bundle oracle
//! (L1.3): the only variable is "native VM vs the same VM compiled to wasm32-wasip1", so any
//! divergence is a wasm-portability bug (width, float formatting, libm, …), not a program bug.
//! The runner's `--sandbox` mode pins the deterministic `SandboxHost`/`SandboxExecutor` pair, so
//! the comparison is well-defined; the expected stderr is composed through the *same* rendering
//! calls the runner makes (`render_mapped` + `render_trace` against the synthetic empty source),
//! keeping the equality structural rather than approximate.
//!
//! External tools are discovered, never assumed: the runner `.wasm` via `NOETA_WASM_RUNNER` (or
//! the workspace-relative release path), wasmtime via `NOETA_WASMTIME` (or `$PATH`). Missing
//! tooling is a loud setup error, not a skip — the oracle keeps the `0 skipped` posture.

use std::path::{Path, PathBuf};
use std::process::Command;

use noeta_backend::RunResult;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::VmBackend;

use crate::collect_cases;

/// The guest-visible path the bundle is handed to the runner under (`--dir <tmp>::/work`). Also
/// the name the runner's synthetic source carries, so the native side renders against the same.
const GUEST_BUNDLE: &str = "/work/case.noeb";

/// The outcome of a wasm differential run over a corpus.
#[derive(Debug, Default)]
pub struct WasmDiffReport {
    /// Programs whose wasm run was byte-identical to the native run.
    pub matched: usize,
    /// Programs outside the VM's current subset (no module to bundle).
    pub skipped: usize,
    /// Programs that did not parse/check cleanly (no module produced).
    pub parse_failed: usize,
    /// Programs whose wasm run diverged from the native run.
    pub failures: Vec<WasmDiffFailure>,
}

/// One program whose wasm run diverged.
#[derive(Debug)]
pub struct WasmDiffFailure {
    pub name: String,
    pub detail: String,
}

impl WasmDiffReport {
    /// Whether every compiled program ran identically under wasm.
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "wasm differential: {} matched, {} skipped (unsupported), {} parse-failed",
            self.matched, self.skipped, self.parse_failed,
        );
        if self.failures.is_empty() {
            out.push_str("every compiled program runs byte-identically under wasm ✓\n");
        } else {
            let _ = writeln!(out, "{} WASM DIVERGENCE(s):", self.failures.len());
            for f in &self.failures {
                let _ = writeln!(out, "  {} — {}", f.name, f.detail);
            }
        }
        out
    }
}

/// The external tooling the oracle drives, discovered once up front.
struct WasmTools {
    wasmtime: PathBuf,
    runner: PathBuf,
    /// The runner's bytes, read once — every stapled case patches a fresh copy.
    runner_bytes: Vec<u8>,
    /// One scratch dir, reused (overwritten) per case — the host side of `/work`.
    workdir: PathBuf,
}

/// Discover wasmtime + the built runner, or explain exactly what is missing.
fn discover_tools() -> Result<WasmTools, String> {
    let wasmtime = std::env::var_os("NOETA_WASMTIME")
        .map(PathBuf::from)
        .or_else(|| which("wasmtime"))
        .ok_or_else(|| {
            "wasmtime not found: install it (https://wasmtime.dev) or set NOETA_WASMTIME"
                .to_string()
        })?;
    let runner = std::env::var_os("NOETA_WASM_RUNNER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // The conformance binary runs from the workspace root; the runner's `wasm-release`
            // build is the deployment artifact, so the oracle exercises exactly what ships.
            PathBuf::from("target/wasm32-wasip1/wasm-release/noeta-wasm-runner.wasm")
        });
    if !runner.is_file() {
        return Err(format!(
            "wasm runner not found at {} — build it first:\n  \
             cargo build -p noeta-wasm-runner --target wasm32-wasip1 --profile wasm-release\n\
             (or set NOETA_WASM_RUNNER)",
            runner.display()
        ));
    }
    let runner_bytes =
        std::fs::read(&runner).map_err(|e| format!("cannot read the wasm runner: {e}"))?;
    let workdir =
        std::env::temp_dir().join(format!("noeta-wasm-differential-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).map_err(|e| format!("cannot create workdir: {e}"))?;
    Ok(WasmTools {
        wasmtime,
        runner,
        runner_bytes,
        workdir,
    })
}

/// A minimal `$PATH` probe (the dev harness has no dependency budget for a `which` crate).
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Run every corpus program under `root` (optionally narrowed to one file) through the wasm
/// runner and compare against the native VM. `Err` is a setup problem (missing tools), distinct
/// from per-program divergences in the report.
pub fn run_wasm_differential(root: &Path, only: Option<&Path>) -> Result<WasmDiffReport, String> {
    let tools = discover_tools()?;
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = WasmDiffReport::default();
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
        {
            continue;
        }
        let name = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        if case.multi {
            match noeta_loader::read_workspace(&case.entry) {
                Ok(raw) => diff_workspace(&name, &raw, &tools, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => diff_single(&name, &text, &tools, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    std::fs::remove_dir_all(&tools.workdir).ok();
    Ok(report)
}

/// Compile one single-file program (gating exactly like the bundle oracle) and diff it.
fn diff_single(name: &str, text: &str, tools: &WasmTools, report: &mut WasmDiffReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source);

    if !noeta_db::tokens(&db, src).0.diagnostics.is_empty()
        || !noeta_db::ast(&db, src).0.diagnostics.is_empty()
    {
        report.parse_failed += 1;
        return;
    }
    if !noeta_db::checked(&db, src).diagnostics.is_empty() {
        // A checker-rejected program produces no bundle — its diagnostics are compile-time, out
        // of the wasm runner's scope; count it matched, mirroring the bundle oracle.
        report.matched += 1;
        return;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => diff_module(name, module, tools, report),
    }
}

/// The workspace analogue of [`diff_single`] for a multi-file fixture.
fn diff_workspace(
    name: &str,
    raw: &noeta_loader::RawWorkspace,
    tools: &WasmTools,
    report: &mut WasmDiffReport,
) {
    let db = LangDatabase::default();
    let ws = noeta_db::workspace(&db, &raw.entry, &raw.modules);

    if noeta_db::linked(&db, ws).0.is_err() {
        report.parse_failed += 1;
        return;
    }
    if !noeta_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => diff_module(name, module, tools, report),
    }
}

/// Run `module` natively and through the wasm runner — in **both** deployment shapes, two-file
/// (W1.1) and stapled single-artifact (W1.2) — folding any divergence into `report`. A case
/// counts as matched only when both shapes match.
fn diff_module(
    name: &str,
    module: &noeta_bytecode::Module,
    tools: &WasmTools,
    report: &mut WasmDiffReport,
) {
    // Native truth: the traced sandbox run — the same semantics the runner executes
    // (SandboxHost + SandboxExecutor, cooperative, trace-collector mode).
    let (native, trace) = VmBackend::new().run_module_traced(module);
    let bundle = noeta_bundle::write(module);

    // Two-file (W1.1): bundle to the shared workdir, handed to the runner as `/work/case.noeb`.
    // The runner module is byte-identical across the corpus, so wasmtime's cache pays off.
    let host_bundle = tools.workdir.join("case.noeb");
    if let Err(e) = std::fs::write(&host_bundle, &bundle) {
        report.failures.push(WasmDiffFailure {
            name: name.to_string(),
            detail: format!("cannot write bundle: {e}"),
        });
        return;
    }
    let two_file = Command::new(&tools.wasmtime)
        .arg("run")
        .args(["-C", "cache=y"])
        .arg(format!("--dir={}::/work", tools.workdir.display()))
        .arg(&tools.runner)
        .arg("--sandbox")
        .arg(GUEST_BUNDLE)
        .output();
    let mut divergence =
        compare(&native, &trace, GUEST_BUNDLE, two_file).map(|d| format!("two-file: {d}"));

    // Stapled (W1.2): the same bundle injected into the runner binary, run with no preopens at
    // all — exactly the shipped single-artifact shape. Every stapled module is unique, so the
    // cache is off (a cold compile is cheap; caching 500+ 2.4 MB variants is not). The guest
    // argv[0] wasmtime reports is the artifact's basename — the runner's synthetic source name.
    if divergence.is_none() {
        divergence = match noeta_bundle::staple_wasm(&tools.runner_bytes, &bundle) {
            Err(e) => Some(format!("stapled: staple_wasm failed: {e}")),
            Ok(image) => {
                let host_wasm = tools.workdir.join("case.wasm");
                match std::fs::write(&host_wasm, &image) {
                    Err(e) => Some(format!("stapled: cannot write artifact: {e}")),
                    Ok(()) => {
                        let stapled = Command::new(&tools.wasmtime)
                            .arg("run")
                            .args(["--env", "NOETA_WASM_SANDBOX=1"])
                            .arg(&host_wasm)
                            .output();
                        compare(&native, &trace, "case.wasm", stapled)
                            .map(|d| format!("stapled: {d}"))
                    }
                }
            }
        };
    }

    match divergence {
        Some(detail) => report.failures.push(WasmDiffFailure {
            name: name.to_string(),
            detail,
        }),
        None => report.matched += 1,
    }
}

/// Compare one wasm execution against the native truth. `source_name` is the synthetic-source
/// name the runner rendered against (the bundle's guest path, or the stapled artifact's
/// basename); the expected stderr is composed through the same rendering calls the runner makes.
fn compare(
    native: &RunResult,
    trace: &[noeta_vm::TraceFrame],
    source_name: &str,
    out: std::io::Result<std::process::Output>,
) -> Option<String> {
    let out = match out {
        Ok(out) => out,
        Err(e) => return Some(format!("wasmtime failed to launch: {e}")),
    };
    let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, source_name, "")]);
    let mut expected_stderr = String::new();
    if !native.diagnostics.is_empty() {
        expected_stderr.push_str(&noeta_diagnostics::render_mapped(
            &sources,
            native.diagnostics.iter(),
        ));
    }
    if trace.len() >= 2 {
        expected_stderr.push_str(&noeta_vm::render_trace(trace, &sources));
    }
    let expected_exit = exit_code_byte(native);

    let wasm_stdout = String::from_utf8_lossy(&out.stdout);
    let wasm_stderr = String::from_utf8_lossy(&out.stderr);
    if wasm_stdout != native.stdout {
        Some(format!(
            "stdout: native {:?}, wasm {:?}",
            native.stdout, wasm_stdout
        ))
    } else if out.status.code() != Some(i32::from(expected_exit)) {
        Some(format!(
            "exit: native {} (as byte {expected_exit}), wasm {:?} — stderr: {wasm_stderr}",
            native.exit_code,
            out.status.code(),
        ))
    } else if wasm_stderr != expected_stderr {
        Some(format!(
            "stderr: native {:?}, wasm {:?}",
            expected_stderr, wasm_stderr
        ))
    } else {
        None
    }
}

/// The process exit byte the runner maps a [`RunResult`] to (`u8::try_from(...).unwrap_or(1)`).
fn exit_code_byte(result: &RunResult) -> u8 {
    u8::try_from(result.exit_code).unwrap_or(1)
}
