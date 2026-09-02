//! `noeta build`/`noeta dump` — compile a program through the shared whole-file pipeline
//! and emit it as a `.noeb` bundle, a stapled executable, a wasm artifact, or a serve component.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_runner::compile::compile_whole_file_with;

use crate::cmd::native::{emit_native, resolve_serve_component, resolve_wasm_runner, runner_base};
use crate::compose;

/// `noeta dump <FILE>` — disassemble the program to its VM bytecode and print it to stdout. Loads,
/// activates any `--tier`/`--target`, type-checks, and compiles through the **same** pipeline as
/// `noeta run` (`compile_real`), so the disassembly is exactly what the VM executes — the tool for
/// inspecting codegen (which ops a construct lowers to, whether a reuse/in-place fast path fired,
/// how names/constants are laid out). A type error prints diagnostics and exits non-zero, like `run`.
pub(crate) fn cmd_dump(
    file: &std::path::Path,
    tiers: &[String],
    target: &Option<String>,
) -> ExitCode {
    // The compose probe hands back the graph it resolved (default selection) so the compile
    // below doesn't resolve it again.
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
    // Same whole-file compile as `run` (so the disassembly is exactly what the VM runs), and a cache
    // participant — a cached module is byte-identical to a fresh compile, so the disassembly matches.
    match compile_whole_file_with(
        file,
        tiers,
        target,
        false,
        resolved.map(|g| noeta_runner::compile::ResolvedFront {
            packages: g.packages,
            package_uses: g.package_uses,
        }),
    ) {
        Ok(compiled) => {
            // Warnings to stderr before the listing to stdout — the two streams stay separable
            // (`noeta dump f.noe > f.txt` still captures only the disassembly).
            crate::output::emit_diagnostics_mapped(&compiled.sources, compiled.warnings.iter());
            print!("{}", compiled.module.disassemble());
            let _ = io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(failure) => failure.report(),
    }
}

/// `noeta build <FILE>` — compile a program to a self-contained artifact. Loads +
/// links, activates any `--tier`/`--target`, type-checks, and compiles through the **same**
/// `compile_real` pipeline as `run`/`dump`. The result is emitted either as a `.noeb` bundle
/// (`noeta_bundle::write`, run by `noeta run app.noeb`) or — with `--exe` — as a self-contained
/// executable that runs the program on its own (`emit_exe`). Either artifact carries no `.noe`
/// source. A type error prints diagnostics and exits non-zero, like `run`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_build(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    exe: bool,
    native: bool,
    wasm: bool,
    serve: bool,
    tiers: &[String],
    target: &Option<String>,
) -> ExitCode {
    // The compose probe hands back the graph it resolved (default selection) so the compile
    // below doesn't resolve it again.
    let resolved = match compose::maybe_delegate(file) {
        Err(code) => return code,
        Ok(resolved) => resolved,
    };
    if usize::from(exe) + usize::from(native) + usize::from(wasm) + usize::from(serve) > 1 {
        eprintln!("noeta: --exe, --native, --wasm, and --serve are mutually exclusive");
        return ExitCode::from(2);
    }
    // Same whole-file compile + startup cache as `run`/`dump`. The emit format doesn't affect the
    // module, so a `build` shares cache entries with a `run` of the same source — each warms the other.
    let module = match compile_whole_file_with(
        file,
        tiers,
        target,
        false,
        resolved.map(|g| noeta_runner::compile::ResolvedFront {
            packages: g.packages,
            package_uses: g.package_uses,
        }),
    ) {
        // A warning does not fail a build — it is reported and the artifact is still produced.
        Ok(compiled) => {
            crate::output::emit_diagnostics_mapped(&compiled.sources, compiled.warnings.iter());
            compiled.module
        }
        Err(failure) => return failure.report(),
    };
    let module = module.as_ref();
    if native {
        emit_native(file, out, module)
    } else if exe {
        emit_exe(file, out, module)
    } else if wasm {
        emit_wasm(file, out, module)
    } else if serve {
        emit_serve(file, out, module)
    } else {
        emit_bundle(file, out, module)
    }
}

/// Emit `module` as a standalone `.noeb` bundle — the default `noeta build` output.
/// Writes to `out` if given, else the input path with a `.noeb` extension.
pub(crate) fn emit_bundle(
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
            eprintln!("noeta: cannot write {}: {err}", out_path.display());
            ExitCode::from(2)
        }
    }
}

/// Emit `module` as a self-contained executable: staple its bundle onto a copy of the
/// lean `noeta-runner` (*not* the toolchain), so the artifact runs the program with no
/// separate `.noeb`, no interpreter, and no dev tooling. Writes to `out` if given, else the input
/// path with its extension stripped (`app.noe` → `app`, or `.exe` on Windows). Marked executable on
/// Unix.
pub(crate) fn emit_exe(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    // The runtime image to embed is the LEAN `noeta-runner` — NOT this CLI. A stapled
    // artifact's argv belongs to the program (it never invokes a CLI verb: `try_run_stapled` fires
    // before arg-parsing), so the toolchain was always dead weight and attack surface here. For an app
    // with native runtime dependencies the base is a *composed* runner (the lean runner + those crates'
    // runtime extensions, dev tooling off); a pure-Noeta app uses the stock runner.
    // Either way it links only app-execution layers (no fmt/LSP/DAP/formatter parsers).
    let runner_path = match runner_base(file) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let runtime = match std::fs::read(&runner_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "noeta: cannot read the lean runtime {}: {err}",
                runner_path.display()
            );
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
            "noeta: refusing to overwrite the source file {}; pass -o <path>",
            file.display()
        );
        return ExitCode::from(2);
    }
    if let Err(err) = std::fs::write(&out_path, &image) {
        eprintln!("noeta: cannot write {}: {err}", out_path.display());
        return ExitCode::from(2);
    }
    // Make the artifact runnable (Unix): rwxr-xr-x.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) =
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755))
        {
            eprintln!(
                "noeta: cannot mark {} executable: {err}",
                out_path.display()
            );
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

/// Emit `module` as a single **wasm** artifact — the `--exe` analogue for the wasm
/// target. A wasm guest cannot read its own binary, so instead of a tail trailer the bundle is
/// injected into the runner's data section and its compiled-in slot patched to point at it
/// (`noeta_bundle::staple_wasm`). Writes to `out` if given, else the input path with a `.wasm`
/// extension. Runs under any WASI runtime: `wasmtime run app.wasm [args…]`.
pub(crate) fn emit_wasm(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let runner_path = match resolve_wasm_runner() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let runner = match std::fs::read(&runner_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "noeta: cannot read the wasm runner {}: {err}",
                runner_path.display()
            );
            return ExitCode::from(2);
        }
    };
    let blob = noeta_bundle::write(module);
    let image = match noeta_bundle::staple_wasm(&runner, &blob) {
        Ok(image) => image,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let out_path = out
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("wasm"));
    match std::fs::write(&out_path, &image) {
        Ok(()) => {
            eprintln!(
                "wrote {} ({} bytes, single wasm artifact)",
                out_path.display(),
                image.len()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: cannot write {}: {err}", out_path.display());
            ExitCode::from(2)
        }
    }
}

/// Emit `module` as a **wasi:http serve component**: staple its bundle into the
/// prebuilt generic `noeta-wasm-serve` component — the exact `--wasm` mechanism one format
/// level up (`staple_wasm` descends into the component's embedded engine module), so no cargo
/// runs at user build time once a generic component exists. Writes to `out` if given, else
/// `<input>.serve.wasm`. Deploy: `wasmtime serve -S cli=y app.serve.wasm`.
pub(crate) fn emit_serve(
    file: &std::path::Path,
    out: Option<&std::path::Path>,
    module: &noeta_bytecode::Module,
) -> ExitCode {
    let component_path = match resolve_serve_component() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let component = match std::fs::read(&component_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!(
                "noeta: cannot read the serve component {}: {err}",
                component_path.display()
            );
            return ExitCode::from(2);
        }
    };
    let blob = noeta_bundle::write(module);
    let image = match noeta_bundle::staple_wasm(&component, &blob) {
        Ok(image) => image,
        Err(err) => {
            eprintln!("noeta: {err}");
            return ExitCode::from(2);
        }
    };
    let out_path = out.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
        let mut name = file.file_stem().unwrap_or_default().to_os_string();
        name.push(".serve.wasm");
        file.with_file_name(name)
    });
    match std::fs::write(&out_path, &image) {
        Ok(()) => {
            eprintln!(
                "wrote {} ({} bytes, wasi:http component — `wasmtime serve -S cli=y {}`)",
                out_path.display(),
                image.len(),
                out_path.display(),
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("noeta: cannot write {}: {err}", out_path.display());
            ExitCode::from(2)
        }
    }
}
