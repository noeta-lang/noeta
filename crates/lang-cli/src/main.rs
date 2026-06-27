//! `lang` — the user-facing toolchain binary.
//!
//! Exposes `run` (execute a file), `test` (run a program's `@test` blocks), and `repl`
//! (interactive); all drive the same pipeline crates, so the binary is thin glue. The binary is
//! named `lang` (placeholder pending the real language name). The conformance corpus / differential
//! / leak harness that tests the *implementation* is a separate dev binary (`lang-conformance`), not
//! a subcommand here — which is what keeps the `lang test` verb free for a user program's own
//! `@test {}` blocks (object-model slice 6).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use clap::{Parser, Subcommand};
use lang_ast::{Expr, Program, Stmt};
use lang_check::TestFn;
use lang_diagnostics::{Diagnostic, DiagnosticCode, render};
use lang_eval::{Session, SessionOutput, TreeWalkBackend};
use lang_lexer::{TokenKind, lex};
use lang_parser::parse;
use lang_span::{Source, SourceId, SourceMap, Span};

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
        /// Activate a dev-tier for this run, e.g. `--tier debug` to compile in `@debug { … }`
        /// blocks (object-model slice 6). Repeatable. Without it, every tier block is stripped.
        /// (The interim active-set interface until build profiles land.)
        #[arg(long)]
        tier: Vec<String>,
    },
    /// Discover and run a program's `@test` blocks (object-model slice 6).
    Test {
        /// Path to a `.lang` file.
        file: PathBuf,
        /// Stop after the first failing test instead of running them all.
        #[arg(long)]
        fail_fast: bool,
        /// Number of tests to run concurrently (default: the machine's parallelism).
        #[arg(long, short)]
        jobs: Option<usize>,
    },
    /// Start an interactive REPL.
    Repl,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run { file, tier } => cmd_run(&file, &tier),
        Command::Test {
            file,
            fail_fast,
            jobs,
        } => cmd_test(&file, fail_fast, jobs),
        Command::Repl => cmd_repl(),
    }
}

/// Type-check and run a program, writing stdout to the real stdout and rendering any diagnostics to
/// stderr — each against the source its span belongs to (via the `SourceMap`). Returns the process
/// exit code. `program` is the loaded program, possibly after dev-tier activation (`cmd_run`).
fn run_program(program: &lang_ast::Program, sources: &SourceMap) -> i32 {
    // The loader already lexed + parsed (and reported any lex/parse errors); type-check then run.
    // One `check_all` produces both the gate diagnostics and the `type_of` site map the backend
    // needs, so the checker runs exactly once (it previously ran again inside the backend).
    let checked = lang_check::check_all(program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(sources, checked.diagnostics.iter());
        return 1;
    }

    match execute_real_host(program, &checked) {
        Ok(result) => {
            print!("{}", result.stdout);
            let _ = io::stdout().flush();
            emit_diagnostics_mapped(sources, result.diagnostics.iter());
            result.exit_code
        }
        Err(err) => {
            eprintln!("lang: {err}");
            1
        }
    }
}

/// Execute an already-checked program against the **real host** (real `env`/`args`, real-disk IO)
/// on a per-isolate tokio runtime (M2.3), returning its [`RunResult`]. It runs the **Core-IR** path
/// — the same drop-annotated IR the conformance reference and the VM execute — so a user's program
/// gets the migration's last-use destruction semantics, not the superseded AST-walk timing. The
/// conformance differential keeps the deterministic sandbox, so this real-host path is never
/// compared backend-to-backend. Shared by `lang run` and the `@test` runner so both execute a
/// program identically. Lowering is total over the parsed language (never `Unsupported`) and purely
/// syntactic, so every loaded program lowers; an `Err` here is only a failure to start the runtime.
fn execute_real_host(
    program: &lang_ast::Program,
    checked: &lang_check::Checked,
) -> Result<lang_backend::RunResult, String> {
    let host =
        lang_runtime::RealHost::new().map_err(|err| format!("cannot start the runtime: {err}"))?;
    // Lower + insert the precise-RC drops exactly as the bytecode pipeline does (with the same
    // destructor-relevance annotation), so this matches the reference, then thread reuse tokens
    // (Phase 5).
    let relevance = lang_ir_passes::Relevance {
        locals: checked.destructor_relevance.locals.clone(),
        params: checked.destructor_relevance.params.clone(),
    };
    let ir = lang_ir::lower(program).expect("Core-IR lowering is total over the parsed language");
    let ir = lang_ir_passes::insert_drops(&ir, Some(&relevance));
    let ir = lang_ir_passes::thread_reuse(&ir);
    Ok(TreeWalkBackend::new().run_ir_with_host(
        program,
        &ir,
        Box::new(host),
        checked.type_of_sites.clone(),
    ))
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

fn cmd_run(file: &std::path::Path, tiers: &[String]) -> ExitCode {
    // Load + link the program: sibling `.lang` modules the entry `use`s are resolved and merged
    // (M1.9); a lone file with no sibling modules links to exactly itself.
    match lang_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            ExitCode::from(2)
        }
        Ok(Ok(linked)) => {
            // With `--tier`, activate those dev-tiers: inline their `@<tier> { … }` blocks (e.g.
            // `@debug`) wherever they appear before checking/running. Without it, the program is run
            // as-is and every tier block is stripped at lowering (the default). Activation borrows
            // nothing from the run, so an owned activated program is produced only when needed.
            if tiers.is_empty() {
                return exit_code(run_program(&linked.program, &linked.sources));
            }
            let active: Vec<&str> = tiers.iter().map(String::as_str).collect();
            let activated = lang_check::activate_tiers(&linked.program, &active);
            if !activated.diagnostics.is_empty() {
                emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
                return ExitCode::from(1);
            }
            exit_code(run_program(&activated.program, &linked.sources))
        }
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            ExitCode::from(1)
        }
    }
}

/// The outcome of running one `@test` fn: whether it passed, the failure message (the first
/// diagnostic, typically the assertion/panic), and anything it wrote to stdout (shown on failure).
struct TestOutcome {
    name: String,
    passed: bool,
    message: Option<String>,
    stdout: String,
}

/// `lang test <FILE>` — discover the program's `@test` blocks (object-model slice 6) and run each
/// as an isolated test. Tests run concurrently (one fresh isolate per test) and, by default, **all**
/// of them run even after a failure; `--fail-fast` stops at the first failure. A test fails when its
/// fn aborts — a false `assert`/`panic` (or any runtime error) — and passes when it returns normally.
/// The program's own top-level "main" effects are not run: `lang test` runs the tests, not the file.
fn cmd_test(file: &std::path::Path, fail_fast: bool, jobs: Option<usize>) -> ExitCode {
    let linked = match lang_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            return ExitCode::from(2);
        }
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => {
            let mut stderr = io::stderr();
            for ld in &load_diagnostics {
                let _ = stderr.write_all(render(&ld.source, &ld.diagnostic).as_bytes());
            }
            return ExitCode::from(1);
        }
    };

    // Activate the `test` tier: inline its `@test` blocks as ordinary top-level declarations and
    // collect the test fns. An unknown-tier block is an E0036 (a typo must not silently vanish).
    let activated = lang_check::activate_tiers(&linked.program, &["test"]);
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check the activated program once, so a broken test is a compile error reported a single
    // time here rather than redundantly inside every per-test run.
    let checked = lang_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    if activated.tests.is_empty() {
        println!("no tests found");
        return ExitCode::SUCCESS;
    }

    // The setup every test shares: the program's declarations (and top-level bindings/globals),
    // with its own "main" effect statements removed. Each test then runs as `setup + <call the test
    // fn>` in a fresh isolate, so the program's `echo`s don't run and one test cannot observe
    // another's state.
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_test_setup(s))
        .cloned()
        .collect();

    let total = activated.tests.len();
    let jobs = jobs
        .filter(|n| *n > 0)
        .unwrap_or_else(default_jobs)
        .min(total);
    println!(
        "running {total} test{} on {jobs} thread{}",
        plural(total),
        plural(jobs),
    );

    let outcomes = run_tests(
        &setup,
        &activated.tests,
        activated.program.span,
        jobs,
        fail_fast,
    );
    report(&outcomes, total)
}

/// Whether a top-level statement is test *setup* — a declaration or a global binding the tests may
/// depend on — as opposed to the program's own "main" effects (which `lang test` does not run).
fn is_test_setup(stmt: &Stmt) -> bool {
    !matches!(
        stmt,
        Stmt::Echo { .. }
            | Stmt::Return { .. }
            | Stmt::If { .. }
            | Stmt::For { .. }
            | Stmt::While { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Expr { .. }
    )
}

/// A statement that calls the zero-arg test fn `name`: `name();`.
fn call_stmt(name: &str, span: Span) -> Stmt {
    Stmt::Expr {
        expr: Expr::Call {
            callee: Box::new(Expr::Ident {
                name: name.to_string(),
                span,
            }),
            args: Vec::new(),
            span,
        },
        span,
    }
}

/// The default test concurrency — the machine's available parallelism (1 if it cannot be queried).
fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Run `tests` concurrently across `jobs` worker threads, each grabbing the next test by an atomic
/// index. By default every test runs; with `fail_fast` a failure sets a shared stop flag and the
/// workers drain out. Results are gathered with their original index and returned in declaration
/// order, so the report is deterministic regardless of completion order.
fn run_tests(
    setup: &[Stmt],
    tests: &[TestFn],
    span: Span,
    jobs: usize,
    fail_fast: bool,
) -> Vec<TestOutcome> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results: Mutex<Vec<(usize, TestOutcome)>> = Mutex::new(Vec::with_capacity(tests.len()));

    thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    if fail_fast && stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= tests.len() {
                        break;
                    }
                    let outcome = run_one_test(setup, &tests[idx], span);
                    let failed = !outcome.passed;
                    results.lock().unwrap().push((idx, outcome));
                    if fail_fast && failed {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            });
        }
    });

    let mut collected = results.into_inner().unwrap();
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, outcome)| outcome).collect()
}

/// Run a single test: synthesize `setup + <call the test fn>`, run it in a fresh real-host isolate,
/// and read a nonzero exit / any diagnostic as a failure (the first diagnostic — the assertion or
/// panic — is the reported message). The synthesized program is a subset of the already-checked
/// activated program plus a call to one of its fns, so it cannot introduce new type errors; if one
/// somehow appears it is surfaced as a failure rather than panicking the worker.
fn run_one_test(setup: &[Stmt], test: &TestFn, span: Span) -> TestOutcome {
    let mut stmts = setup.to_vec();
    stmts.push(call_stmt(&test.name, test.span));
    let program = Program { stmts, span };

    let checked = lang_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return TestOutcome {
            name: test.name.clone(),
            passed: false,
            message: Some(checked.diagnostics[0].message.clone()),
            stdout: String::new(),
        };
    }

    match execute_real_host(&program, &checked) {
        Ok(result) => {
            let passed = result.exit_code == 0 && result.diagnostics.is_empty();
            let message = (!passed).then(|| {
                result
                    .diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| format!("exited with code {}", result.exit_code))
            });
            TestOutcome {
                name: test.name.clone(),
                passed,
                message,
                stdout: result.stdout,
            }
        }
        Err(err) => TestOutcome {
            name: test.name.clone(),
            passed: false,
            message: Some(err),
            stdout: String::new(),
        },
    }
}

/// Print the per-test report and the summary, returning the process exit code (success only when
/// every test ran and passed). Failing tests show their message and any captured stdout.
fn report(outcomes: &[TestOutcome], total: usize) -> ExitCode {
    let mut passed = 0usize;
    for outcome in outcomes {
        if outcome.passed {
            passed += 1;
            println!("  ok    {}", outcome.name);
        } else {
            println!("  FAIL  {}", outcome.name);
            if let Some(message) = &outcome.message {
                println!("        {message}");
            }
            for line in outcome.stdout.lines() {
                println!("        | {line}");
            }
        }
    }

    let failed = outcomes.len() - passed;
    let not_run = total - outcomes.len();
    println!();
    if not_run > 0 {
        println!(
            "{passed} passed, {failed} failed, {not_run} not run (stopped early), {total} total"
        );
    } else {
        println!("{passed} passed, {failed} failed, {total} total");
    }
    let _ = io::stdout().flush();

    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
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
