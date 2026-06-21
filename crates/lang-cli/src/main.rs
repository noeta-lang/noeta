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
use lang_conformance::{Stage, run_corpus, run_differential};
use lang_diagnostics::{Diagnostic, DiagnosticCode, render};
use lang_eval::{Backend, Session, SessionOutput, TreeWalkBackend};
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

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
            dir,
        } => {
            if differential {
                cmd_differential(file.as_deref(), &dir)
            } else {
                cmd_test(json, file.as_deref(), stage.as_deref(), &dir)
            }
        }
    }
}

/// Compile and run a source, writing stdout to the real stdout and rendering any
/// diagnostics to stderr. Returns the process exit code.
fn run_source(source: &Source) -> i32 {
    let lexed = lex(source);
    let parsed = parse(source, &lexed.tokens);

    let compile_diagnostics: Vec<&Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .collect();
    if !compile_diagnostics.is_empty() {
        emit_diagnostics(source, compile_diagnostics.into_iter());
        return 1;
    }

    // Type-check before running (M1.7): reject type-incorrect programs at compile time.
    let type_diagnostics = lang_check::check(&parsed.program);
    if !type_diagnostics.is_empty() {
        emit_diagnostics(source, type_diagnostics.iter());
        return 1;
    }

    let result = TreeWalkBackend::new().run(&parsed.program);
    print!("{}", result.stdout);
    let _ = io::stdout().flush();
    emit_diagnostics(source, result.diagnostics.iter());
    result.exit_code
}

fn emit_diagnostics<'a>(source: &Source, diagnostics: impl Iterator<Item = &'a Diagnostic>) {
    let mut stderr = io::stderr();
    for diagnostic in diagnostics {
        let _ = stderr.write_all(render(source, diagnostic).as_bytes());
    }
}

fn cmd_run(file: &std::path::Path) -> ExitCode {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
    };
    let source = Source::new(SourceId::FIRST, file.display().to_string(), text);
    exit_code(run_source(&source))
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

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
