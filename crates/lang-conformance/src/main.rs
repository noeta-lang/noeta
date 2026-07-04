//! `lang-conformance` — the development harness that tests the **language implementation** against
//! its `.noe` corpus: the expectation runner, the differential oracle (VM vs tree-walker), and the
//! leak oracle. This is a dev-only tool (it ships test fixtures and cross-checks two backends), so it
//! is a **separate binary** from the user-facing `lang` CLI — which keeps the `lang test` verb free
//! for running a user program's own `@test {}` blocks (object-model slice 6).
//!
//! Invoke with `cargo run -p lang-conformance -- [flags]`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use lang_conformance::{Stage, run_corpus, run_differential, run_leak_check};

#[derive(Parser)]
#[command(
    name = "lang-conformance",
    version,
    about = "Run the lang conformance corpus (development harness)"
)]
struct Cli {
    /// Emit machine-readable JSON instead of human text.
    #[arg(long)]
    json: bool,
    /// Only run cases whose path ends with this (e.g. `orders/empty.noe`).
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    /// Run only through this pipeline stage: `lexer`, `parser`, or `eval`.
    #[arg(long, value_name = "STAGE")]
    stage: Option<String>,
    /// Cross-check the M1 bytecode VM against the M0 tree-walker (the differential oracle) instead
    /// of running expectations. Programs the VM cannot compile yet are skipped; any divergence on a
    /// compiled program fails.
    #[arg(long)]
    differential: bool,
    /// Run the leak oracle: execute every corpus program on both backends and report any heap still
    /// live after it returns (residency 0 is the goal). Exits non-zero if any program leaks.
    #[arg(long)]
    check_leaks: bool,
    /// Run the JIT differential oracle (milestone P-JIT): execute every compilable corpus program
    /// through the interpreter *and* the forced tier-1 JIT and assert byte-identical results plus
    /// zero heap residency under JIT. Requires the `jit` build feature. Any divergence or leak fails.
    #[arg(long)]
    jit_differential: bool,
    /// The corpus root directory.
    #[arg(long, default_value = "tests/conformance")]
    dir: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.jit_differential {
        cmd_jit_differential(cli.file.as_deref(), &cli.dir)
    } else if cli.check_leaks {
        cmd_leaks(cli.file.as_deref(), &cli.dir)
    } else if cli.differential {
        cmd_differential(cli.file.as_deref(), &cli.dir)
    } else {
        cmd_test(
            cli.json,
            cli.file.as_deref(),
            cli.stage.as_deref(),
            &cli.dir,
        )
    }
}

fn cmd_test(
    json: bool,
    file: Option<&std::path::Path>,
    stage: Option<&str>,
    dir: &std::path::Path,
) -> ExitCode {
    let stage = match stage {
        Some(name) => match Stage::parse(name) {
            Some(stage) => stage,
            None => {
                eprintln!(
                    "lang-conformance: unknown stage `{name}` (expected lexer, parser, or eval)"
                );
                return ExitCode::from(2);
            }
        },
        None => Stage::default(),
    };

    let report = run_corpus(dir, file, stage);

    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_human());
    }

    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the differential oracle over the corpus: the M1 VM cross-checked against the M0 tree-walker.
/// Exits non-zero only on a genuine divergence (skipped/unsupported programs do not fail).
fn cmd_differential(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    let report = run_differential(dir, file);
    print!("{}", report.to_human());
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the leak oracle over the corpus: every program executes on both backends and any heap still
/// live after it returns is reported (architecture §0). Exits non-zero if any program leaks.
fn cmd_leaks(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    let report = run_leak_check(dir, file);
    print!("{}", report.to_human());
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the JIT differential oracle (P-JIT): interpreter vs forced tier-1 JIT over the corpus. Only
/// available in a `--features jit` build; otherwise report that and exit non-zero so a plain build
/// cannot silently "pass" a gate it never ran.
#[cfg(feature = "jit")]
fn cmd_jit_differential(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    let report = lang_conformance::run_jit_differential(dir, file);
    print!("{}", report.to_human());
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Fallback when the binary was built without the `jit` feature: the oracle cannot run.
#[cfg(not(feature = "jit"))]
fn cmd_jit_differential(_file: Option<&std::path::Path>, _dir: &std::path::Path) -> ExitCode {
    eprintln!(
        "lang-conformance: --jit-differential requires the `jit` feature \
         (build with `cargo run -p lang-conformance --features jit -- --jit-differential`)"
    );
    ExitCode::from(2)
}
