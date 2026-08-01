//! The stock `noeta` binary: [`noeta_cli::run_cli`] with no extra extension units. A composed
//! toolchain (package-manager Phase 3) is a generated peer of this file in its own crate, passing
//! the app's native extension units instead of `&[]`.

use std::process::ExitCode;

fn main() -> ExitCode {
    noeta_runner::compile::phase_stop("main");
    noeta_cli::run_cli(&[], &[])
}
