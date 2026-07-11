//! The wasm runner (P-WASM W1.1): a `wasm32-wasip1` binary that runs a `.noeb` bundle on the
//! bytecode VM.
//!
//! This is the wasm analogue of a `noeta build --exe` artifact — VM on embedded bytecode, no
//! compiler, no source — except the bundle arrives as a WASI-preopened file (two-file
//! deployment: `wasmtime --dir . noeta-wasm-runner.wasm app.noeb`). The single-artifact form
//! (`noeta build --wasm` injecting the bundle into this binary's data) is W1.2.
//!
//! Hosts: [`noeta_wasi_host::WasiHost`] by default (the real WASI world), or the deterministic
//! `SandboxHost` under `--sandbox` — the configuration the wasm differential oracle (W1.3) runs,
//! asserting this runner byte-identical to a native run. Execution is cooperative
//! (single-threaded, `SandboxExecutor`) — wasm has no OS threads, and async leaves degrade
//! serial-but-correct exactly as they do on any host without a real executor.
//!
//! The crate is deliberately **target-agnostic** — it builds and behaves identically on native,
//! which is how its integration test drives it; nothing is `cfg(target_family = "wasm")`-gated.

use std::io::Write;
use std::process::ExitCode;

use noeta_span::{Source, SourceId, SourceMap};

fn usage() -> ExitCode {
    eprintln!("usage: noeta-wasm-runner [--sandbox] <app.noeb> [args...]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    // `--sandbox` (the oracle configuration) precedes the bundle path; everything from the bundle
    // path on is the program's argument vector, `noeta run`'s `program_args` shape:
    // `[<bundle>, <pass-through…>]`.
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
    let module = match noeta_bundle::read(&bytes) {
        Ok(module) => module,
        Err(err) => {
            eprintln!("noeta-wasm-runner: cannot load {bundle_path}: {err}");
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

    // A bundle ships no source: diagnostics and tracebacks render against a synthetic empty
    // source (message/code/file:line show, no snippet) — the `noeta run app.noeb` convention.
    let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, &bundle_path, "")]);
    if !result.diagnostics.is_empty() {
        let rendered = noeta_diagnostics::render_mapped(&sources, result.diagnostics.iter());
        let _ = std::io::stderr().write_all(rendered.as_bytes());
    }
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, &sources));
    }

    ExitCode::from(u8::try_from(result.exit_code).unwrap_or(1))
}
