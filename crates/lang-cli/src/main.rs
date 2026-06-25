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
use lang_lexer::{TokenKind, lex};
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
    // tokio runtime (M2.3). It runs the **Core-IR** path — the same drop-annotated IR the
    // conformance reference and the VM execute — so a user's program gets the migration's last-use
    // destruction semantics, not the superseded AST-walk timing. The conformance differential keeps
    // the deterministic sandbox, so this real-host path is never compared backend-to-backend.
    let host = match lang_runtime::RealHost::new() {
        Ok(host) => host,
        Err(err) => {
            eprintln!("lang: cannot start the runtime: {err}");
            return 1;
        }
    };
    // Lower + insert the precise-RC drops exactly as the bytecode pipeline does (with the same
    // destructor-relevance annotation), so `lang run` matches the reference, then thread reuse
    // tokens (Phase 5). There is no AST-walker fallback: lowering is total over the parsed language
    // (it never produces `Unsupported`) and is purely syntactic, so every loaded program lowers —
    // and a fallback would be a second, divergent destruction semantics.
    let relevance = lang_ir_passes::Relevance {
        locals: checked.destructor_relevance.locals.clone(),
        params: checked.destructor_relevance.params.clone(),
    };
    let ir = lang_ir::lower(&linked.program)
        .expect("Core-IR lowering is total over the parsed language");
    let ir = lang_ir_passes::insert_drops(&ir, Some(&relevance));
    let ir = lang_ir_passes::thread_reuse(&ir);
    let result = TreeWalkBackend::new().run_ir_with_host(
        &linked.program,
        &ir,
        Box::new(host),
        checked.type_of_sites,
    );
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

    eprintln!("type :help for commands");
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Skip blank lines when nothing is pending.
        if buffer.is_empty() && line.trim().is_empty() {
            eprint!("» ");
            let _ = io::stderr().flush();
            continue;
        }
        // A `:`-prefixed line (when nothing is pending) is a REPL meta-command — tooling that lives
        // outside the language grammar (`:type`, `:drop`, `:bindings`, `:reset`, `:help`, `:quit`).
        if buffer.is_empty() && line.trim_start().starts_with(':') {
            if repl_meta(&mut session, line.trim()) == MetaOutcome::Quit {
                break;
            }
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

/// Whether a meta-command asked to leave the REPL.
#[derive(PartialEq)]
enum MetaOutcome {
    Continue,
    Quit,
}

/// Handle a `:`-prefixed REPL meta-command. These are REPL *tooling*, deliberately outside the
/// language grammar (the language itself has no manual `drop`/`type` keyword): the REPL keeps
/// top-level bindings alive across entries — extended lifetime, unlike compiled code's last-use
/// destruction — so `:drop` is how a destructor is observed or an object reclaimed interactively,
/// and `:type` reports a value's runtime type in a session that runs no checker.
fn repl_meta(session: &mut Session, line: &str) -> MetaOutcome {
    let body = line.strip_prefix(':').unwrap_or(line);
    let mut parts = body.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "quit" | "q" => return MetaOutcome::Quit,
        "help" | "h" | "?" => print_repl_help(),
        "reset" => {
            session.reset();
            eprintln!("(session reset)");
        }
        "bindings" | "b" => {
            let names = session.binding_names();
            if names.is_empty() {
                eprintln!("(no bindings)");
            } else {
                println!("{}", names.join(", "));
                let _ = io::stdout().flush();
            }
        }
        "drop" | "free" => {
            if arg.is_empty() {
                eprintln!("usage: :drop <name>");
            } else {
                let (found, out) = session.drop_binding(arg);
                print!("{}", out.stdout);
                let _ = io::stdout().flush();
                if found {
                    eprintln!("(dropped `{arg}`)");
                } else {
                    eprintln!("no binding named `{arg}`");
                }
            }
        }
        "type" | "t" => {
            if arg.is_empty() {
                eprintln!("usage: :type <expr>");
            } else {
                repl_type(session, arg);
            }
        }
        other => eprintln!("unknown command `:{other}` — try :help"),
    }
    MetaOutcome::Continue
}

/// `:type <expr>` — parse `expr`, evaluate it in the session, and print its runtime type.
fn repl_type(session: &mut Session, expr: &str) {
    let source = Source::new(SourceId::FIRST, "<repl-type>", format!("{expr};"));
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let diags: Vec<Diagnostic> = lexed
        .diagnostics
        .iter()
        .chain(parsed.diagnostics.iter())
        .cloned()
        .collect();
    if !diags.is_empty() {
        emit_diagnostics(&source, diags.iter());
        return;
    }
    let out = session.type_of(&parsed.program);
    print!("{}", out.stdout);
    let _ = io::stdout().flush();
    if !out.diagnostics.is_empty() {
        emit_diagnostics(&source, out.diagnostics.iter());
    } else if let Some(ty) = out.value {
        println!("{ty}");
        let _ = io::stdout().flush();
    }
}

fn print_repl_help() {
    eprintln!("REPL commands:");
    eprintln!("  :type <expr>   show the runtime type of an expression (evaluates it)");
    eprintln!("  :drop <name>   run a binding's destructor now and unbind it (alias :free)");
    eprintln!("  :bindings      list the live bindings");
    eprintln!("  :reset         clear all bindings and start fresh");
    eprintln!("  :help          show this help");
    eprintln!("  :quit          exit the REPL (or Ctrl-D)");
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

    // An entry with unclosed `(`/`{`/`[` is a multi-line definition still being typed (a `class`,
    // a `fn` body, a multi-line list/object literal). The parser may report a *non*-end-of-input
    // error inside such a buffer rather than cleanly running out of tokens, so the end-of-input
    // check below is not enough on its own — gather more lines until the delimiters balance. The
    // count is over lexer tokens, so braces inside string/template literals (a single token) and
    // `${…}` interpolation never miscount.
    if unclosed_delimiters(&lexed.tokens) {
        return ReplStep::Incomplete;
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

/// Whether `tokens` has more opening than closing delimiters — i.e. a `(`/`{`/`[` left unclosed, the
/// signature of a multi-line REPL entry still being typed. A single net depth across all three kinds
/// is enough to decide *incompleteness* (the parser validates correct nesting once the buffer is
/// balanced); a buffer that closes more than it opens (net ≤ 0) is left to the parser to report.
fn unclosed_delimiters(tokens: &[lang_lexer::Token]) -> bool {
    let mut depth: i32 = 0;
    for token in tokens {
        match token.kind {
            TokenKind::LParen | TokenKind::LBrace | TokenKind::LBracket => depth += 1,
            TokenKind::RParen | TokenKind::RBrace | TokenKind::RBracket => depth -= 1,
            _ => {}
        }
    }
    depth > 0
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

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<lang_lexer::Token> {
        lex(&Source::new(SourceId::FIRST, "<t>", src.to_string())).tokens
    }

    #[test]
    fn unclosed_delimiters_detects_in_progress_multiline_entries() {
        // Open `{`/`(`/`[` with no match → still being typed (a `class`, `fn` body, or literal).
        assert!(unclosed_delimiters(&toks("class Res {")));
        assert!(unclosed_delimiters(&toks(
            "fn run(): void {\n  mut r = Res.new(3);"
        )));
        assert!(unclosed_delimiters(&toks("[1,\n 2,")));
        assert!(unclosed_delimiters(&toks("f(")));
        // Balanced (or over-closed) → let the parser decide, not "incomplete".
        assert!(!unclosed_delimiters(&toks("class Res { id: int }")));
        assert!(!unclosed_delimiters(&toks("[1, 2, 3]")));
        assert!(!unclosed_delimiters(&toks("x = 5;")));
        assert!(!unclosed_delimiters(&toks("}")));
        // Braces inside a template string are one token — they never miscount.
        assert!(!unclosed_delimiters(&toks("echo \"drop ${id}\";")));
    }
}
