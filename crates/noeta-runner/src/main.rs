//! The lean production runtime binary (dev-deps D3): the native analogue of `noeta-wasm-runner`.
//! It runs a `.noeb` bundle on the real host — as a **stapled** artifact (`noeta build --exe`
//! injects the bundle onto this binary) or in **two-file** form (`noeta-runner app.noeb [args…]`).
//!
//! It links only the app-execution layers (VM + real `Host` + runtime extensions, via the shared
//! `noeta_runner` lib) and **nothing from the dev toolchain (L3)**: no fmt, no formatter/parser
//! (`malva`), no LSP/DAP/MCP. That exclusion is *structural* — this crate does not depend on those
//! crates, so it is auditable by its `Cargo.toml`, not by tracing `#[cfg]`s. Running `.noe` **source**
//! (the PHP-style deploy) is the next slice (D3c); today the binary requires a pre-built bundle.

use std::process::ExitCode;

// mimalloc as the global allocator (matching the CLI's shipped binary): a production runtime wants
// the same allocator characteristics its programs were tuned against.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn usage() -> ExitCode {
    eprintln!("usage: noeta-runner <app.noeb> [args...]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    // A stapled artifact: the bundle rides this binary and the whole real argv is the program's.
    // The lean runner uses the executable-file-stem p2p namespace (no `noeta.toml` lookup — a
    // shipped binary is not run from its source tree).
    if let Some(code) = noeta_runner::try_run_stapled(|_| None) {
        return code;
    }

    // Two-file mode: everything from the bundle path on is the program's argument vector — the same
    // `[<bundle>, <pass-through…>]` shape `noeta run` presents through `args.all()`.
    let program_argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(bundle_path) = program_argv.first().cloned() else {
        return usage();
    };

    let bytes = match std::fs::read(&bundle_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("noeta-runner: cannot read {bundle_path}: {err}");
            return ExitCode::from(2);
        }
    };
    if !noeta_bundle::is_bundle(&bytes) {
        eprintln!(
            "noeta-runner: {bundle_path} is not a `.noeb` bundle (built by `noeta build`); source execution arrives in D3c"
        );
        return ExitCode::from(2);
    }
    noeta_runner::run_bundle_bytes(
        std::path::Path::new(&bundle_path),
        &bytes,
        program_argv,
        None,
        false,
    )
}
