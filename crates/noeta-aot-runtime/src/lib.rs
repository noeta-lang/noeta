//! P-AOT L3.2b(3): the **AOT runtime** — the static library a `noeta build --native` program links
//! against to become a self-contained native executable.
//!
//! A native artifact is laid out exactly like a Level-2 stapled exe: `[linked runtime | bundle |
//! trailer]`. The difference is the runtime half. An L2 exe embeds the whole `noeta` toolchain and
//! interprets the bundle; an L3 native exe embeds *this* lean runtime plus the program's prototypes
//! **compiled to native code**, wired through the linker-defined [`noeta_aot_dispatch`] table.
//!
//! At startup the C-ABI [`main`] entry:
//!   1. reads its own executable's tail to recover the stapled bundle (the L2 mechanism),
//!   2. decodes it back into a [`Module`],
//!   3. binds the linker-resolved `noeta_aot_dispatch` table into the VM's per-prototype entry
//!      tables and runs on the real host — so eligible prototypes dispatch straight to the native
//!      bodies linked into this binary, and the rest interpret, exactly as the in-process proof
//!      ([`noeta_vm`]'s `aot_bound_dispatch_runs_native_in_process`) showed.
//!
//! The native bodies call back into the runtime through the `noeta_jit_*` helper symbols this crate
//! re-exports via `noeta-vm`'s `aot` feature; the linker resolves those against this archive.

use std::process::ExitCode;

use noeta_bytecode::Module;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::VmBackend;

// The linker-defined dispatch table (P-AOT L3.2a): `[count][main_0, fast_0, main_1, fast_1, …]`,
// pointer-width words the linker resolved to the real code addresses of the AOT-compiled prototype
// bodies (or null for an interpreted prototype / a prototype with no fast body). The program's AOT
// object *defines* this symbol; here we only reference it. Its true size is the whole table — we
// declare it as a single word and take the object's address (offset 0, the `count`), then read the
// rest through that pointer, the standard interop shape for an opaquely-sized symbol.
#[allow(unsafe_code)]
unsafe extern "C" {
    static noeta_aot_dispatch: usize;
}

/// Install extra native **runtime** extensions, then recover and run the embedded program, returning
/// its C exit status — the extension seam a **native-dependency** `noeta build --native` links against
/// (dev-deps: the `--native` analogue of the composed runner). When a shipped app depends on packages
/// with native tier handlers or modules, `--native` composes an AOT-runtime staticlib that aggregates
/// those crates' `NOETA_EXTENSIONS` and calls this, so the VM resolves the native modules/types/tiers
/// the AOT-compiled bundle references. The units are *runtime* capabilities only — a composed AOT
/// runtime carries no more dev tooling than the plain one (each mixed crate is built with its formatter
/// feature off). The plain [`main`] passes an empty slice, giving exactly today's std-only behavior.
///
/// Maps the process `ExitCode` to a C `int`: `ExitCode` has no getter, so the honest bridge reports
/// success as 0 and any failure as 1 — the AOT differential compares stdout + success, and the
/// program's own `exit_code` is already folded into `run`'s return.
pub fn run_embedded_with_extensions(
    units: &'static [&'static (dyn noeta_stdlib::Extension + Sync)],
) -> core::ffi::c_int {
    // Seed the default registry with std + the app's native units before any lookup (the VM's first
    // module/type/tier resolution reads it). An empty slice is exactly the lazy std-only default.
    noeta_stdlib::registry::install_with_extras(units);
    match run() {
        c if c == ExitCode::SUCCESS => 0,
        _ => 1,
    }
}

/// C-runtime entry point of a native AOT binary. `crt0` calls this after process setup; it returns
/// the program's exit code. Declared `extern "C"` and exported as `main` so the system linker
/// resolves `crt0`'s reference to it. Arguments (`argc`/`argv`) are read through `std::env` — on the
/// platforms this interim runtime targets, std's startup captures them independently of this entry —
/// so the signature takes none.
///
/// Gated behind the default **`entry`** feature so a *composed* AOT runtime (a native-dependency app's
/// `--native` base) can depend on this crate with `default-features = false` and provide its **own**
/// `main` — which installs the app's extensions via [`run_embedded_with_extensions`] — without a
/// duplicate-`main` link error. A plain `--native` (no native deps) keeps this entry.
///
/// # Safety
/// Invoked by the C runtime exactly once, as the process entry; not to be called from Rust.
#[cfg(feature = "entry")]
#[unsafe(no_mangle)]
#[allow(unsafe_code)]
pub unsafe extern "C" fn main() -> core::ffi::c_int {
    run_embedded_with_extensions(&[])
}

/// Recover the embedded program, run it native-bound, and print its output — the body of [`main`],
/// split out so it deals in an ordinary [`ExitCode`]. Any failure to locate/decode the bundle is a
/// corrupt-artifact error on stderr (this binary is *only* ever a stapled artifact; there is no
/// "plain toolchain" fallback as in the CLI).
fn run() -> ExitCode {
    let module = match load_embedded_module() {
        Ok(module) => module,
        Err(err) => {
            eprintln!("noeta: cannot load embedded program: {err}");
            return ExitCode::from(2);
        }
    };
    let (result, trace) = run_native(std::sync::Arc::new(module));

    // A source-free artifact renders runtime aborts against a synthetic empty source: message +
    // code + location show, but there is no snippet — the honest cost of shipping no `.noe` (same as
    // the L1/L2 bundle runner).
    let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, "<aot>", "")]);
    print!("{}", result.stdout);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    for diagnostic in &result.diagnostics {
        eprint!(
            "{}",
            noeta_diagnostics::render(sources.source(diagnostic.span.source), diagnostic)
        );
    }
    if trace.len() >= 2 {
        eprint!("{}", noeta_vm::render_trace(&trace, &sources));
    }
    ExitCode::from(result.exit_code as u8)
}

/// Read this executable's stapled trailer to recover the embedded bundle, and decode it to a
/// [`Module`]. Mirrors the CLI's `try_run_stapled`, but this binary is *always* a stapled artifact,
/// so a missing/short trailer is an error rather than a "run the normal CLI" signal.
fn load_embedded_module() -> Result<Module, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let image = std::fs::read(&exe).map_err(|e| e.to_string())?;
    let bundle = noeta_bundle::extract_stapled(&image)
        .ok_or_else(|| "no stapled bundle found on this executable".to_string())?;
    noeta_bundle::read(bundle).map_err(|e| e.to_string())
}

/// Run `module` against the real host with its AOT-compiled prototypes bound in (P-AOT L3.2b). A
/// per-isolate factory mints a fresh real host + wall-clock executor (isolates I.4b), exactly as the
/// CLI's `run_module_real_host` does; the extra step here is binding the linker-resolved
/// [`noeta_aot_dispatch`] table so eligible prototypes dispatch to native code.
fn run_native(module: std::sync::Arc<Module>) -> (noeta_vm::RunResult, Vec<noeta_vm::TraceFrame>) {
    let factory: noeta_vm::IsolateFactory = std::sync::Arc::new(|| {
        let host: Box<dyn noeta_stdlib::Host> =
            Box::new(noeta_host_real::RealHost::new().expect("cannot start an isolate's runtime"));
        let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
            noeta_host_real::RealExecutor::new().expect("cannot start an isolate's async executor"),
        );
        (host, executor)
    });
    let (host, executor) = factory();
    // The address of the linker-defined table's first word (the count). `&raw const` avoids forming a
    // reference to the single-word declaration when the real object spans the whole table.
    #[allow(unsafe_code)]
    let dispatch: *const usize = &raw const noeta_aot_dispatch;
    // SAFETY: `dispatch` points at the AOT dispatch table this binary's own program object defined
    // and the linker resolved; its function pointers live in this executable's text, valid for the
    // whole run. `run_module_aot`'s contract is exactly that.
    #[allow(unsafe_code)]
    unsafe {
        VmBackend::new().run_module_aot(module, dispatch, host, executor, factory)
    }
}
