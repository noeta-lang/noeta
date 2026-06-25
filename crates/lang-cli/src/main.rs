//! `lang` — the toolchain binary.
//!
//! M0 exposes three subcommands: `run` (execute a file), `repl` (interactive), and
//! `test` (run the conformance corpus). All three drive the same pipeline crates, so
//! the binary is thin glue. The binary is named `lang` (placeholder pending the real
//! language name).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use lang_conformance::{Stage, run_corpus, run_differential, run_leak_check};
use lang_diagnostics::{Diagnostic, DiagnosticCode, render};
use lang_eval::{Session, SessionOutput, TreeWalkBackend};
use lang_lexer::lex;
use lang_loader::Linked;
use lang_parser::parse;
use lang_span::{Source, SourceId, SourceMap};

#[derive(Parser)]
#[command(name = "lang", version, about = "The lang toolchain (working title)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a program file.
    Run {
        /// Path to a `.lang` file.
        file: PathBuf,
    },
    /// Start an interactive REPL.
    Repl,
    /// Run the conformance corpus.
    Test {
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
        /// Only run cases whose path ends with this (e.g. `orders/empty.lang`).
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Run only through this pipeline stage: `lexer`, `parser`, or `eval`.
        #[arg(long, value_name = "STAGE")]
        stage: Option<String>,
        /// Cross-check the M1 bytecode VM against the M0 tree-walker (the differential
        /// oracle) instead of running expectations. Programs the VM cannot compile yet are
        /// skipped; any divergence on a compiled program fails.
        #[arg(long)]
        differential: bool,
        /// Run the leak oracle: execute every corpus program on both backends and report any
        /// heap still live after it returns (residency 0 is the goal). Exits non-zero if any
        /// program leaks — including the known nested-closure cycles Phase 6 will reap.
        #[arg(long)]
        check_leaks: bool,
        /// The corpus root directory.
        #[arg(long, default_value = "tests/conformance")]
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run { file } => cmd_run(&file),
        Command::Repl => cmd_repl(),
        Command::Test {
            json,
            file,
            stage,
            differential,
            check_leaks,
            dir,
        } => {
            if check_leaks {
                cmd_leaks(file.as_deref(), &dir)
            } else if differential {
                cmd_differential(file.as_deref(), &dir)
            } else {
                cmd_test(json, file.as_deref(), stage.as_deref(), &dir)
            }
        }
    }
}

/// Type-check and run an already-loaded, linked program, writing stdout to the real stdout and
/// rendering any diagnostics to stderr — each against the source its span belongs to (via the
/// linked program's `SourceMap`). Returns the process exit code.
fn run_linked(linked: &Linked) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `check_all` produces both the gate diagnostics and the `type_of` site map the backend
    // needs, so the checker runs exactly once (it previously ran again inside the backend).
    let checked = lang_check::check_all(&linked.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return 1;
    }

    // `lang run` executes against the real host (real `env`/`args`, real-disk IO) on a per-isolate
    // tokio runtime (M2.3). The conformance differential keeps the deterministic sandbox, so this
    // real-host path is never compared backend-to-backend.
    let result = match lang_runtime::RealHost::new() {
        Ok(host) => TreeWalkBackend::new().run_with_host_sites(
            &linked.program,
            Box::new(host),
            checked.type_of_sites,
        ),
        Err(err) => {
            eprintln!("lang: cannot start the runtime: {err}");
            return 1;
        }
    };
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    emit_diagnostics_mapped(&linked.sources, result.diagnostics.iter());
    result.exit_code
}

fn emit_diagnostics<'a>(source: &Source, diagnostics: impl Iterator<Item = &'a Diagnostic>) {
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let _ = stderr.write_all(render(source, diagnostic).as_bytes());
    }
}

/// Render each diagnostic against the source it belongs to (resolved by its span's `SourceId`
/// through the [`SourceMap`]), so a diagnostic on a declaration merged in from a sibling module
/// renders against that module's file and text rather than the entry's.
fn emit_diagnostics_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) {
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let source = sources.source(diagnostic.span.source);
        let _ = stderr.write_all(render(source, diagnostic).as_bytes());
    }
}

fn cmd_run(file: &std::path::Path) -> ExitCode {
    // Load + link the program: sibling `.lang` modules the entry `use`s are resolved and merged
    // (M1.9); a lone file with no sibling modules links to exactly itself.
    match lang_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            ExitCode::from(2)
        }
        Ok(Ok(linked)) => exit_code(run_linked(&linked)),
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            ExitCode::from(1)
        }
    }
}

/// Whether an entry was consumed (evaluated or reported) or is still incomplete and needs
/// more input (multiline continuation).
enum ReplStep {
    Consumed,
    Incomplete,
}

fn cmd_repl() -> ExitCode {
    let stdin = io::stdin();
    let mut session = Session::new();
    let mut buffer = String::new();
    let mut entry_no: u32 = 0;
    eprint!("lang repl — type a statement, Ctrl-D to exit\n» ");
    let _ = io::stderr().flush();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Skip blank lines when nothing is pending.
        if buffer.is_empty() && line.trim().is_empty() {
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(&line);

        match repl_step(&mut session, &buffer, &mut entry_no) {
            ReplStep::Consumed => {
                buffer.clear();
                eprint!("» ");
            }
            // Keep the buffer and read another line; show a continuation prompt.
            ReplStep::Incomplete => eprint!("… "),
        }
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
}

/// Try to evaluate the accumulated REPL buffer. Statements ending in `;`/`}` evaluate as-is;
/// a bare expression (no trailing `;`) is retried with a `;` appended so its value can be
/// printed. If the only parse problem is hitting end-of-input, the entry is treated as
/// incomplete and more input is requested (multiline). Any other error is reported, and the
/// buffer is reset so one bad entry cannot wedge the session.
fn repl_step(session: &mut Session, buffer: &str, entry_no: &mut u32) -> ReplStep {
    let source = Source::new(
        SourceId::FIRST,
        format!("<repl:{entry_no}>"),
        buffer.to_string(),
    );
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let diags: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .collect();

    if diags.is_empty() {
        *entry_no += 1;
        emit_session(&source, session.eval(&parsed.program));
        return ReplStep::Consumed;
    }

    // A bare expression needs a terminating `;`; retry with one appended.
    let patched = format!("{buffer};");
    let psource = Source::new(SourceId::FIRST, format!("<repl:{entry_no}>"), patched);
    let plexed = lex(&psource);
    let pparsed = parse(&psource, &plexed.tokens);
    if plexed.diagnostics.is_empty() && pparsed.diagnostics.is_empty() {
        *entry_no += 1;
        emit_session(&psource, session.eval(&pparsed.program));
        return ReplStep::Consumed;
    }

    // Only end-of-input errors → the entry is unfinished; gather more lines.
    if diags
        .iter()
        .all(|d| d.code == DiagnosticCode::UnexpectedEndOfInput)
    {
        return ReplStep::Incomplete;
    }

    // A genuine syntax error: report it against the original buffer and reset.
    *entry_no += 1;
    emit_diagnostics(&source, diags.iter());
    ReplStep::Consumed
}

/// Print a session evaluation's stdout, the value of a trailing bare expression (if any),
/// then any diagnostics.
fn emit_session(source: &Source, out: SessionOutput) {
    print!("{}", out.stdout);
    if let Some(value) = out.value {
        println!("{value}");
    }
    let _ = io::stdout().flush();
    emit_diagnostics(source, out.diagnostics.iter());
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
                eprintln!("lang: unknown stage `{name}` (expected lexer, parser, or eval)");
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

/// Run the differential oracle over the corpus: the M1 VM cross-checked against the M0
/// tree-walker. Exits non-zero only on a genuine divergence (skipped/unsupported programs
/// do not fail).
fn cmd_differential(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    let report = run_differential(dir, file);
    print!("{}", report.to_human());
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Run the leak oracle over the corpus: every program executes on both backends and any heap
/// still live after it returns is reported (architecture §0). Exits non-zero if any program
/// leaks — the authoritative regression gate (with its known-debt allowlist) is the conformance
/// corpus test; this is the developer-facing inspection view.
fn cmd_leaks(file: Option<&std::path::Path>, dir: &std::path::Path) -> ExitCode {
    let report = run_leak_check(dir, file);
    print!("{}", report.to_human());
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
