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
use lang_conformance::{Stage, run_corpus};
use lang_diagnostics::{Diagnostic, render};
use lang_eval::{Backend, TreeWalkBackend};
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
            dir,
        } => cmd_test(json, file.as_deref(), stage.as_deref(), &dir),
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

fn cmd_repl() -> ExitCode {
    let stdin = io::stdin();
    let mut line_no: u32 = 0;
    eprint!("lang repl — type a statement, Ctrl-D to exit\n» ");
    let _ = io::stderr().flush();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if !line.trim().is_empty() {
            line_no += 1;
            let source = Source::new(SourceId::FIRST, format!("<repl:{line_no}>"), line);
            run_source(&source);
        }
        eprint!("» ");
        let _ = io::stderr().flush();
    }
    eprintln!();
    ExitCode::SUCCESS
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

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
