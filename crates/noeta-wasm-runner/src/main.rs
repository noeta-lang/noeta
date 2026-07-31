//! The wasm runner (P-WASM W1.1/W1.2): a `wasm32-wasip1` binary that runs a `.noeb` bundle on
//! the bytecode VM.
//!
//! This is the wasm analogue of a `noeta build --exe` artifact — VM on embedded bytecode, no
//! compiler, no source — in two deployment shapes:
//!
//! - **Stapled (single artifact, W1.2)**: `noeta build --wasm` injects the bundle into this
//!   binary's data section (see [`embedded`]); the whole argv belongs to the program, exactly
//!   like a stapled `--exe` binary invoked directly.
//! - **Two-file (W1.1)**: the bundle arrives as a WASI-preopened file
//!   (`wasmtime --dir . noeta-wasm-runner.wasm app.noeb`).
//!
//! Hosts: [`noeta_wasi_host::WasiHost`] by default (the real WASI world), or the deterministic
//! `SandboxHost` under `--sandbox` / `NOETA_WASM_SANDBOX=1` — the configuration the wasm
//! differential oracle (W1.3) runs, asserting this runner byte-identical to a native run. (The
//! env form exists because a stapled artifact's argv belongs to the program, so a flag cannot
//! claim it.) Execution is cooperative (single-threaded, `SandboxExecutor`) — wasm has no OS
//! threads, and async leaves degrade serial-but-correct exactly as they do on any host without a
//! real executor.
//!
//! The crate is target-agnostic — it builds and behaves identically on native, which is how its
//! integration tests drive both shapes; nothing is `cfg(target_family = "wasm")`-gated.

mod embedded;

use std::io::Write;
use std::process::ExitCode;

use noeta_span::{Source, SourceId, SourceMap};

fn usage() -> ExitCode {
    eprintln!("usage: noeta-wasm-runner [--sandbox] <app.noeb> [args...]");
    ExitCode::from(2)
}

/// The oracle configuration via environment — the only channel a stapled artifact has.
fn sandbox_env() -> bool {
    std::env::var_os("NOETA_WASM_SANDBOX").is_some_and(|v| v == "1")
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();

    // A stapled artifact (W1.2): the bundle rides the binary and the whole real argv is the
    // program's — same convention as a `noeta build --exe` binary invoked directly.
    if let Some(bytes) = embedded::bundle() {
        let name = argv
            .first()
            .cloned()
            .unwrap_or_else(|| "app.wasm".to_string());
        return run(bytes, &name, argv, sandbox_env());
    }

    // Two-file mode: `--sandbox` (the oracle configuration) precedes the bundle path; everything
    // from the bundle path on is the program's argument vector, `noeta run`'s `program_args`
    // shape: `[<bundle>, <pass-through…>]`.
    let mut rest = argv[1..].iter().peekable();
    let sandbox = rest.peek().is_some_and(|a| *a == "--sandbox");
    if sandbox {
        rest.next();
    }
    let program_argv: Vec<String> = rest.cloned().collect();
    let Some(bundle_path) = program_argv.first().cloned() else {
        return usage();
    };

    let bytes = match std::fs::read(&bundle_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("noeta-wasm-runner: cannot read {bundle_path}: {err}");
            return ExitCode::from(2);
        }
    };
    if !noeta_bundle::is_bundle(&bytes) {
        eprintln!(
            "noeta-wasm-runner: {bundle_path} is not a `.noeb` bundle (built by `noeta build`)"
        );
        return ExitCode::from(2);
    }
    run(&bytes, &bundle_path, program_argv, sandbox || sandbox_env())
}

/// Decode `bytes` and run the module: the shared tail of the stapled and two-file paths.
/// `name` labels the synthetic source diagnostics render against; `program_argv` is what the
/// program observes through `args.all()`.
fn run(bytes: &[u8], name: &str, program_argv: Vec<String>, sandbox: bool) -> ExitCode {
    // Seed the process-default extension registry with the std units. On native this happens for
    // free: `noeta_stdlib` registers a fallback provider from a `#[ctor]`, so merely linking the
    // crate gives the process a working default. That ctor is `cfg(not(target_family = "wasm"))` —
    // its doc states that "every wasm driver assembles its registry explicitly", which the
    // playground engine does and this runner did not. Unseeded, `default_registry()` answers `None`
    // and every registry-keyed lookup silently takes its fallback path: `construct`'s native
    // fielded-type resolution built `std.http.Frame` under the *qualified* name instead of the
    // canonical short shape name and skipped the stamped reflected identity, so a constructed
    // native value narrowed to `none` and compared unequal to the identical literal. Seeding here
    // covers both entry shapes (stapled and two-file), before anything can look a name up.
    noeta_stdlib::registry::default_seeded();

    let module = match noeta_bundle::read(bytes) {
        Ok(module) => module,
        Err(err) => {
            eprintln!("noeta-wasm-runner: cannot load {name}: {err}");
            return ExitCode::from(2);
        }
    };

    let host: Box<dyn noeta_stdlib::Host> = if sandbox {
        // The oracle configuration: the exact deterministic world a native differential run uses
        // (fixture env/args and all), so W1.3 can assert byte-identity. No argv override — the
        // sandbox's fixed args fixture IS the determinism.
        Box::new(noeta_stdlib::SandboxHost::new())
    } else {
        Box::new(noeta_wasi_host::WasiHost::new().with_args(program_argv))
    };
    let executor: Box<dyn noeta_stdlib::Executor> = Box::new(noeta_stdlib::SandboxExecutor::new());

    // `run_module_debug(…, None)` is documented as exactly the plain no-JIT run plus the abort
    // traceback — cooperative isolates, tier-0. That is precisely the wasm shape: no `jit`
    // feature exists in this build, and wasm has no threads for real isolates.
    let (result, trace) =
        noeta_vm::VmBackend::new().run_module_debug(&module, host, executor, None);

    print!("{}", result.stdout);
    let _ = std::io::stdout().flush();

    // The program's OWN stderr stream (`std.io`'s `err`/`errln`), buffered into `RunResult` exactly
    // as stdout is, and written FIRST — the order every run tail uses: program stderr, then
    // diagnostics, then the traceback (`noeta_runner::run_compiled_module`). Omitting it silently
    // dropped every `err`/`errln` byte a wasm-hosted program wrote.
    let _ = std::io::stderr().write_all(result.stderr.as_bytes());

    // A bundle ships no source: diagnostics and tracebacks render against a synthetic empty
    // source (message/code/file:line show, no snippet) — the `noeta run app.noeb` convention.
    let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, name, "")]);
    if !result.diagnostics.is_empty() {
        let rendered = noeta_diagnostics::render_mapped(&sources, result.diagnostics.iter());
        let _ = std::io::stderr().write_all(rendered.as_bytes());
    }
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, &sources));
    }

    ExitCode::from(u8::try_from(result.exit_code).unwrap_or(1))
}
