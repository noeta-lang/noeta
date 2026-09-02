//! The **AOT runtime** — the static library a `noeta build --native` program links
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

use noeta_bytecode::Module;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::VmBackend;

// The linker-defined dispatch table: `[count][main_0, fast_0, main_1, fast_1, …]`,
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
/// Returns the program's own status byte, widened to a C `int`.
///
/// It used to return `ExitCode`, mapped `SUCCESS => 0` and everything else to **1** — because
/// `ExitCode` has no getter, so once the code is inside one the number is gone. The comment here
/// justified that by saying the AOT differential "compares stdout + success"; then row 9 built an
/// AOT differential that compares the whole `RunResult`, and `std/os_exit.noe` — `os.exit(3)` —
/// came back as 1. Fixing the `as u8` truncation in the tail was not enough on its own, because
/// this collapse sits downstream of it: `RunTail::status()` had the right byte and `emit()` threw
/// it away one call later. The tail now hands over `emit_status()` and nothing narrows it again.
pub fn run_embedded_with_extensions(
    units: &'static [&'static (dyn noeta_stdlib::Extension + Sync)],
) -> core::ffi::c_int {
    // The startup work a C-ABI entry skips (see `restore_rust_signal_defaults`). First, because a
    // program that writes before it is done is a program that can already have died.
    restore_rust_signal_defaults();
    // Seed the default registry with std + the app's native units before any lookup (the VM's first
    // module/type/tier resolution reads it). An empty slice is exactly the lazy std-only default.
    noeta_stdlib::registry::install_with_extras(units);
    core::ffi::c_int::from(run())
}

/// Ignore `SIGPIPE`, which is what Rust's own startup does and what this entry point skips.
///
/// Every *other* Noeta execution surface is an ordinary Rust binary, so `crt0` calls Rust's
/// `lang_start`, which calls `sys::unix::init`, which sets `SIGPIPE` to `SIG_IGN` before `main`
/// runs. That is why `std::io` can report a write to a closed pipe as an ordinary
/// `ErrorKind::BrokenPipe` value: the signal that would otherwise have killed the process first is
/// disarmed.
///
/// A `--native` artifact exports its own C-ABI [`main`], so `crt0` calls *it* and `lang_start` never
/// runs. `SIGPIPE` therefore kept its default disposition — terminate — and the artifact died on
/// signal 13 with no exit code, no stderr and no traceback wherever `noeta run` returned an `Err`.
/// Two ordinary things did it:
///
/// * `./app | head -3` — the everyday shell pipeline. `noeta run … | head -3` exits 0; the same
///   program built `--native` was killed by `SIGPIPE` as soon as `head` closed the pipe.
/// * `Process.try_write` to a child that has exited (`std/os_try_write.noe`). The whole point of the
///   recoverable write door is that a broken pipe is expressible as a value — and it is, on every
///   surface except the one that ships. This is how the bug surfaced: the AOT differential's native
///   side aborted with `exit None` on CI, where the spawned `echo` reliably won the race to exit,
///   and passed on an idle machine, where the write reached the pipe buffer first.
///
/// Children are unaffected: `std::process::Command` resets `SIGPIPE` to `SIG_DFL` in the child
/// between `fork` and `exec`, exactly as it does under `noeta run`.
#[cfg(unix)]
fn restore_rust_signal_defaults() {
    // SAFETY: `signal(2)` with `SIG_IGN` installs no handler, so there is no async-signal-safety
    // obligation to discharge; this runs on the entry thread before any other thread exists. It is
    // the same call, with the same argument, that `std`'s startup makes on this platform — the
    // point is to restore that state, not to choose a new one. A failure is not actionable (the
    // disposition simply stays as inherited), so the result is dropped rather than aborting a run.
    #[allow(unsafe_code)]
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }
}

/// No-op off Unix: `SIGPIPE` is a POSIX concept, and Windows' `crt0` hands control to `main` with no
/// equivalent disposition to restore.
#[cfg(not(unix))]
fn restore_rust_signal_defaults() {}

/// C-runtime entry point of a native AOT binary. `crt0` calls this after process setup; it returns
/// the program's exit code. Declared `extern "C"` and exported as `main` so the system linker
/// resolves `crt0`'s reference to it. Arguments (`argc`/`argv`) are read through `std::env` — on the
/// platforms this interim runtime targets, std's startup captures them independently of this entry —
/// so the signature takes none.
///
/// Argv is not the only thing Rust's `lang_start` would have done on the way here, and it is the one
/// piece that survives being skipped. The rest does not: see `restore_rust_signal_defaults`, which
/// puts back the `SIGPIPE` disposition whose absence killed artifacts on signal 13. Anything else
/// this entry point is later found to owe `lang_start` belongs there, beside it, for the same reason.
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
/// split out so it deals in an ordinary status byte. Any failure to locate/decode the bundle is a
/// corrupt-artifact error on stderr (this binary is *only* ever a stapled artifact; there is no
/// "plain toolchain" fallback as in the CLI).
fn run() -> u8 {
    let module = match load_embedded_module() {
        Ok(module) => module,
        Err(err) => {
            eprintln!("noeta: cannot load embedded program: {err}");
            return 2;
        }
    };
    let (result, trace) = run_native(std::sync::Arc::new(module));

    // A source-free artifact renders runtime aborts against a synthetic empty source: message +
    // code + location show, but there is no snippet — the honest cost of shipping no `.noe` (same as
    // the L1/L2 bundle runner).
    let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, "<aot>", "")]);
    // The shared run epilogue (audit row 1). This tail was hand-rolled and wrong three ways: it
    // dropped the program's own `stderr` stream entirely, it rendered diagnostics one-by-one
    // through `render` instead of `render_mapped` (so a multi-source span resolved against the
    // wrong file), and it converted the exit code with `as u8` — which made a program exiting 256
    // exit **0**, a failure reported as a success.
    noeta_backend::RunTail::render_colored(
        &result,
        &trace,
        &sources,
        noeta_diagnostics::stderr_color(),
    )
    .emit_status()
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

/// Run `module` against the real host with its AOT-compiled prototypes bound in. A
/// per-isolate factory mints a fresh real host + wall-clock executor (isolates I.4b), exactly as the
/// CLI's `run_module_real_host` does; the extra step here is binding the linker-resolved
/// [`noeta_aot_dispatch`] table so eligible prototypes dispatch to native code.
///
/// The host runs with **live output** (`with_live_output(true)`), like every other foreground run
/// surface. Without it a `--native` binary buffered its whole stdout until exit, so a long-running
/// or killed program showed nothing at all — exactly what made a slow run indistinguishable from a
/// hung one on the paths that already turned it on (audit row 1).
fn run_native(module: std::sync::Arc<Module>) -> (noeta_vm::RunResult, Vec<noeta_vm::TraceFrame>) {
    let factory: noeta_vm::IsolateFactory = std::sync::Arc::new(|| {
        let host: Box<dyn noeta_stdlib::Host> = Box::new(
            noeta_host_real::RealHost::new()
                .expect("cannot start an isolate's runtime")
                .with_live_output(true),
        );
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
