//! `noeta-conformance` — the development harness that tests the **language implementation** against
//! its `.noe` corpus: the expectation runner, the differential oracle (VM vs tree-walker), and the
//! leak oracle. This is a dev-only tool (it ships test fixtures and cross-checks two backends), so it
//! is a **separate binary** from the user-facing `lang` CLI — which keeps the `lang test` verb free
//! for running a user program's own `@test {}` blocks (object-model slice 6).
//!
//! Invoke with `cargo run -p noeta-conformance -- [flags]`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use noeta_conformance::{Stage, run_corpus, run_differential, run_leak_check};

#[derive(Parser)]
#[command(
    name = "noeta-conformance",
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
    /// With `--jit-differential`: arm the forced-JIT run with a **never-set** cancellation flag, so
    /// the JIT emits its loop-header cancellation poll (isolate-cancel, JIT half) on every compiled
    /// body. The program must still produce a byte-identical result — that is the whole claim the
    /// poll rests on. Without this flag the oracle covers the production, poll-free codegen.
    #[arg(long)]
    cancel_poll: bool,
    /// With `--jit-differential`: emit **AOT-form** bodies from the forced JIT (inline caches off,
    /// null call sites, no cancellation poll) — the codegen `noeta build --native` links, run
    /// in-process. The cheap arm of the AOT oracle: same corpus, same full-`RunResult` comparison,
    /// no linker. Mutually exclusive with `--cancel-poll` (they are two different codegen shapes).
    #[arg(long)]
    aot_bodies: bool,
    /// Run the **linked** AOT differential oracle (parallel-path audit row 9): for every corpus
    /// program, AOT-compile it, `cc`-link it against the real `libnoeta_aot.a`, staple its bundle on,
    /// and assert the artifact's stdout, stderr and exit code match the interpreted run through the
    /// same runtime entry. Needs a C toolchain (`NOETA_CC`, else `cc`) and the runtime archive
    /// (`NOETA_AOT_RUNTIME_LIB`, else built here). Minutes, not seconds — the gate arm.
    #[arg(long)]
    aot_differential: bool,
    /// Run the wasm differential oracle (P-WASM W1.3): compile every corpus program to a `.noeb`
    /// and execute it through the wasm runner under wasmtime (`--sandbox`), asserting stdout,
    /// exit code, and rendered stderr byte-identical to the native VM. Needs `wasmtime` (or
    /// `NOETA_WASMTIME`) and a built runner (`cargo build -p noeta-wasm-runner --target
    /// wasm32-wasip1 --release`, or `NOETA_WASM_RUNNER`). Any divergence fails.
    #[arg(long)]
    wasm_differential: bool,
    /// The corpus root directory.
    #[arg(long, default_value = "tests/conformance")]
    dir: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // A `--file` that names nothing is a typo, and every oracle below would otherwise report the
    // empty run as a pass — "0 passed, 0 failed, 0 total", exit 0. That is the same green-looking
    // nothing the whole-corpus guards already refuse, arriving through the one door they exempt,
    // and it is worse here because it is what an agent runs to *verify a fix*: the narrowed check
    // is the evidence, so a mistyped filter turns "I confirmed it" into a statement about nothing.
    //
    // Distinct from the run executing zero *programs*, which stays legitimate for a narrowed
    // differential whose one case the checker rejects. This asks only whether the file exists.
    if let Some(only) = cli.file.as_deref()
        && noeta_conformance::cases_selected(&cli.dir, only) == 0
    {
        eprintln!(
            "noeta-conformance: --file `{}` matches no case under {} — refusing to report an \
             empty run as a pass.",
            only.display(),
            cli.dir.display()
        );
        return ExitCode::from(2);
    }
    if cli.jit_differential {
        if cli.cancel_poll && cli.aot_bodies {
            eprintln!(
                "noeta-conformance: --cancel-poll and --aot-bodies are two different codegen \
                 shapes; run them as two arms"
            );
            return ExitCode::from(2);
        }
        cmd_jit_differential(
            cli.file.as_deref(),
            &cli.dir,
            cli.cancel_poll,
            cli.aot_bodies,
        )
    } else if cli.aot_differential {
        cmd_aot_differential(cli.file.as_deref(), &cli.dir)
    } else if cli.wasm_differential {
        cmd_wasm_differential(cli.file.as_deref(), &cli.dir)
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
                    "noeta-conformance: unknown stage `{name}` (expected lexer, parser, or eval)"
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

/// Run the wasm differential oracle (P-WASM W1.3): the wasm runner under wasmtime vs the native
/// VM over the corpus. Missing tooling (wasmtime / built runner) is a loud setup error and exit 2
/// — never a silent pass.
fn cmd_wasm_differential(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    match noeta_conformance::run_wasm_differential(dir, file) {
        Ok(report) => {
            print!("{}", report.to_human());
            // A whole-corpus run that executed NOTHING is a broken harness, not a pass: a wrong
            // working directory, an empty `--dir`, or a corpus that stopped compiling would
            // otherwise print "0 ran and agreed" and exit 0 — the same green-looking nothing that
            // let this oracle sit red behind a SKIP. A `--file` run is exempt: narrowing to one
            // case that the checker rejects legitimately runs zero programs.
            if file.is_none() && report.matched == 0 {
                eprintln!(
                    "noeta-conformance: --wasm-differential ran ZERO programs over {} — \
                     that is a broken harness, not a pass (wrong --dir?).",
                    dir.display()
                );
                return ExitCode::FAILURE;
            }
            if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(setup) => {
            eprintln!("noeta-conformance: --wasm-differential setup failed: {setup}");
            ExitCode::from(2)
        }
    }
}

/// Run the JIT differential oracle (P-JIT): interpreter vs forced tier-1 JIT over the corpus. Only
/// available in a `--features jit` build; otherwise report that and exit non-zero so a plain build
/// cannot silently "pass" a gate it never ran.
#[cfg(feature = "jit")]
fn cmd_jit_differential(
    file: Option<&std::path::Path>,
    dir: &std::path::Path,
    cancel_poll: bool,
    aot_bodies: bool,
) -> ExitCode {
    let arm = match (cancel_poll, aot_bodies) {
        (true, _) => noeta_conformance::JitDiffArm::CancelPoll,
        (_, true) => noeta_conformance::JitDiffArm::AotBodies,
        _ => noeta_conformance::JitDiffArm::Plain,
    };
    let report = noeta_conformance::run_jit_differential_with(dir, file, arm);
    print!("{}", report.to_human());
    // A whole-corpus arm that ran ZERO programs is a broken harness, not a pass — the posture the
    // wasm oracle had to learn (it sat green at `0 skipped` while executing nothing).
    if file.is_none() && report.matched == 0 {
        eprintln!(
            "noeta-conformance: --jit-differential ran ZERO programs over {} — that is a broken \
             harness, not a pass (wrong --dir?).",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the **linked** AOT differential oracle (parallel-path audit row 9): every corpus program as a
/// real `noeta build --native` artifact, against the interpreted run through the same runtime entry.
/// Missing tooling (a C toolchain, the runtime archive) is a loud setup error and exit 2 — never a
/// silent pass, which is exactly what the one hand-written `--native` test did.
#[cfg(feature = "jit")]
fn cmd_aot_differential(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    match noeta_conformance::run_aot_differential(dir, file) {
        Ok(report) => {
            print!("{}", report.to_human());
            // A floor, not a zero-check. "Ran nothing" is the failure mode the wasm oracle shipped,
            // but "ran a tenth of the corpus" is the same failure wearing a number: a change that
            // makes 700 programs stop compiling would otherwise pass here while covering almost
            // nothing. Measured at 801 on 2026-08-01; raise it when it climbs, and if it drops, find
            // out which programs stopped running before touching this line.
            const MIN_CASES: usize = 760;
            if file.is_none() && report.matched < MIN_CASES {
                eprintln!(
                    "noeta-conformance: --aot-differential compared only {} programs over {} \
                     (floor {MIN_CASES}) — that is a broken harness or a corpus that stopped \
                     compiling, not a pass.",
                    report.matched,
                    dir.display(),
                );
                return ExitCode::FAILURE;
            }
            if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(setup) => {
            eprintln!("noeta-conformance: --aot-differential setup failed: {setup}");
            ExitCode::from(2)
        }
    }
}

/// Fallback when the binary was built without the `jit` feature: there is no AOT codegen to test.
#[cfg(not(feature = "jit"))]
fn cmd_aot_differential(_file: Option<&std::path::Path>, _dir: &std::path::Path) -> ExitCode {
    eprintln!(
        "noeta-conformance: --aot-differential requires the `jit` feature \
         (build with `cargo run -p noeta-conformance --features jit -- --aot-differential`)"
    );
    ExitCode::from(2)
}

/// Fallback when the binary was built without the `jit` feature: the oracle cannot run.
#[cfg(not(feature = "jit"))]
fn cmd_jit_differential(
    _file: Option<&std::path::Path>,
    _dir: &std::path::Path,
    _cancel_poll: bool,
    _aot_bodies: bool,
) -> ExitCode {
    eprintln!(
        "noeta-conformance: --jit-differential requires the `jit` feature \
         (build with `cargo run -p noeta-conformance --features jit -- --jit-differential`)"
    );
    ExitCode::from(2)
}
