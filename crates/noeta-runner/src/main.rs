//! The lean production runtime binary (dev-deps D3): the native analogue of `noeta-wasm-runner`.
//! It runs a `.noeb` bundle on the real host — as a **stapled** artifact (`noeta build --exe`
//! injects the bundle onto this binary) or in **two-file** form (`noeta-runner app.noeb [args…]`).
//!
//! It also runs a `.noe` **source** file directly (PHP-style deploy) — compiling on the fly through
//! the same L2 pipeline the CLI uses — so a source tree can be deployed and run without the toolchain.
//!
//! It links only the app-execution layers (VM + real `Host` + runtime extensions + the compile
//! front-end, via the shared `noeta_runner` lib) and **nothing from the dev toolchain (L3)**: no fmt,
//! no formatter/parser (`malva`), no LSP/DAP/MCP. That exclusion is *structural* — this crate does not
//! depend on those crates, so it is auditable by its `Cargo.toml`, not by tracing `#[cfg]`s.

use std::process::ExitCode;

// mimalloc as the global allocator (matching the CLI's shipped binary): a production runtime wants
// the same allocator characteristics its programs were tuned against.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn usage() -> ExitCode {
    eprintln!("usage: noeta-runner <app.noe | app.noeb> [args...]");
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

    let path = std::path::Path::new(&bundle_path);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("noeta-runner: cannot read {bundle_path}: {err}");
            return ExitCode::from(2);
        }
    };
    // A pre-built `.noeb` bundle runs directly; anything else is treated as `.noe` source and
    // compiled on the fly (the PHP-style deploy). Tiers/target come from the entry's `noeta.toml`
    // (no `--tier`/`--target` flags — a shipped runner takes only the program's argv), and the
    // startup cache is honoured exactly as `noeta run` does. `app_id = None` → executable-file-stem
    // p2p namespace.
    if noeta_bundle::is_bundle(&bytes) {
        noeta_runner::run_bundle_bytes(path, &bytes, program_argv, None, false)
    } else {
        noeta_runner::run_source_file(path, &[], &None, false, program_argv, None, false)
    }
}
