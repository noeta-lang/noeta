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
use std::time::Instant;

use clap::{Parser, Subcommand};
use lang_ast::{AttrArg, AttrValue, Expr, Program, Stmt};
use lang_check::TierFn;
use lang_diagnostics::{Diagnostic, DiagnosticCode, render};
use lang_eval::{Session, SessionOutput, TreeWalkBackend};
use lang_lexer::{TokenKind, lex};
use lang_parser::parse;
use lang_span::{Source, SourceId, SourceMap, Span};

mod manifest;

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
        /// (The interim active-set interface, complementary to `--profile`.)
        #[arg(long)]
        tier: Vec<String>,
        /// Activate the tiers a build profile makes live (from `lang.toml`), e.g.
        /// `--profile dev`. Unioned with any `--tier`.
        #[arg(long)]
        profile: Option<String>,
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
        /// Run only tests tagged `#[Group("<name>")]` with this group.
        #[arg(long)]
        group: Option<String>,
        /// Only run when the `test` tier is live in this `lang.toml` build profile; otherwise the
        /// runner does nothing.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Discover and run a program's `@bench` blocks, measuring each (object-model slice 6).
    Bench {
        /// Path to a `.lang` file.
        file: PathBuf,
        /// Override the iteration count for every benchmark, taking precedence over a per-bench
        /// `@bench(iterations: N)` directive. Without either, a default count is used.
        #[arg(long)]
        iterations: Option<u64>,
        /// Only run when the `bench` tier is live in this `lang.toml` build profile; otherwise the
        /// runner does nothing.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Extract a program's `@doc { … }` text blocks to stdout (object-model slice 6).
    Doc {
        /// Path to a `.lang` file.
        file: PathBuf,
        /// Only extract when the `doc` tier is live in this `lang.toml` build profile; otherwise
        /// nothing is emitted.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Start an interactive REPL.
    Repl,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            file,
            tier,
            profile,
        } => cmd_run(&file, &tier, &profile),
        Command::Test {
            file,
            fail_fast,
            jobs,
            group,
            profile,
        } => cmd_test(&file, fail_fast, jobs, &group, &profile),
        Command::Bench {
            file,
            iterations,
            profile,
        } => cmd_bench(&file, iterations, &profile),
        Command::Doc { file, profile } => cmd_doc(&file, &profile),
        Command::Repl => cmd_repl(),
    }
}

/// For a tier runner: whether its `tier` is live under `--profile`. `Ok(true)` when no profile was
/// given (the runner always runs); `Ok(false)` when a profile was given but does not make `tier`
/// live (the runner should no-op); `Err` on a profile-resolution failure (a fatal error the caller
/// prints).
fn tier_active_in_profile(
    entry: &std::path::Path,
    profile: &Option<String>,
    tier: &str,
) -> Result<bool, String> {
    match profile {
        None => Ok(true),
        Some(name) => Ok(manifest::resolve_active_tiers(entry, name)?
            .iter()
            .any(|t| t == tier)),
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
    // Lower with the checker's `List<packed>` map so packed-list literals stream into a flat buffer
    // (P-PACK 2.5); the layout rides on the IR, so `run_ir_with_host` needs no separate map.
    let ir = lang_ir::lower_with_packed(program, &checked.packed_list_sites)
        .expect("Core-IR lowering is total over the parsed language");
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

fn cmd_run(file: &std::path::Path, tiers: &[String], profile: &Option<String>) -> ExitCode {
    // The active tier set is the union of any `--profile`'s live tiers (from `lang.toml`) and any
    // explicit `--tier` flags, resolved before loading so a bad profile fails fast.
    let mut active: Vec<String> = match profile {
        Some(name) => match manifest::resolve_active_tiers(file, name) {
            Ok(tiers) => tiers,
            Err(err) => {
                eprintln!("lang: {err}");
                return ExitCode::from(1);
            }
        },
        None => Vec::new(),
    };
    for tier in tiers {
        if !active.contains(tier) {
            active.push(tier.clone());
        }
    }

    // Load + link the program: sibling `.lang` modules the entry `use`s are resolved and merged
    // (M1.9); a lone file with no sibling modules links to exactly itself.
    match lang_loader::load(file) {
        Err(err) => {
            eprintln!("lang: cannot read {}: {err}", file.display());
            ExitCode::from(2)
        }
        Ok(Ok(linked)) => {
            // Activate the resolved dev-tiers: inline their `@<tier> { … }` blocks (e.g. `@debug`)
            // wherever they appear before checking/running. With no active tiers the program is run
            // as-is and every tier block is stripped at lowering (the default). Activation borrows
            // nothing from the run, so an owned activated program is produced only when needed.
            if active.is_empty() {
                return exit_code(run_program(&linked.program, &linked.sources));
            }
            let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();
            let activated = lang_check::activate_tiers(&linked.program, &active_refs);
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

/// Gate a tier runner on `--profile`: if a profile was given and does not make `tier` live, print a
/// note and return the success exit code (the runner no-ops); on a resolution failure, print it and
/// return the error code. `None` means "proceed" (no profile gate). The caller runs its body only
/// when this returns `None`.
fn profile_gate(entry: &std::path::Path, profile: &Option<String>, tier: &str) -> Option<ExitCode> {
    match tier_active_in_profile(entry, profile, tier) {
        Ok(true) => None,
        Ok(false) => {
            println!(
                "tier `{tier}` is not active in profile `{}`",
                profile.as_deref().unwrap_or_default()
            );
            Some(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("lang: {err}");
            Some(ExitCode::from(1))
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
fn cmd_test(
    file: &std::path::Path,
    fail_fast: bool,
    jobs: Option<usize>,
    group: &Option<String>,
    profile: &Option<String>,
) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "test") {
        return code;
    }
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
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    // The `--group` filter (object-model slice 6h): keep only tests tagged `#[Group("<g>")]`.
    let selected: Vec<&TierFn> = match group {
        Some(g) => activated
            .tests
            .iter()
            .filter(|t| test_group(t).as_deref() == Some(g.as_str()))
            .collect(),
        None => activated.tests.iter().collect(),
    };
    if selected.is_empty() {
        match group {
            Some(g) => println!("no tests in group `{g}`"),
            None => println!("no tests found"),
        }
        return ExitCode::SUCCESS;
    }

    // Partition into skipped (`#[Skip]`) and runnable. A skipped test is reported but never run, and
    // never fails the suite (a skipped `#[Data]` test counts as one skip, not one per row).
    let (skipped_refs, runnable): (Vec<&TierFn>, Vec<&TierFn>) =
        selected.into_iter().partition(|t| test_is_skipped(t));
    let skipped: Vec<String> = skipped_refs.iter().map(|t| skip_label(t)).collect();

    // Expand each runnable test into its case(s): a `#[Data([…])]` test runs once per row (reported
    // `name[row]`); an ordinary test is a single zero-arg case.
    let cases: Vec<TestCase> = runnable.iter().flat_map(|t| test_cases(t)).collect();
    let total = cases.len() + skipped.len();
    let run_count = cases.len();
    let jobs = jobs
        .filter(|n| *n > 0)
        .unwrap_or_else(default_jobs)
        .min(run_count.max(1));
    let skipped_note = if skipped.is_empty() {
        String::new()
    } else {
        format!(", {} skipped", skipped.len())
    };
    println!(
        "running {run_count} test{} on {jobs} thread{}{skipped_note}",
        plural(run_count),
        plural(jobs),
    );

    let outcomes = run_tests(&setup, &cases, activated.program.span, jobs, fail_fast);
    report(&outcomes, &skipped, total)
}

/// One runnable test invocation: which fn to call, the report label, and an optional argument (a
/// `#[Data]` row — `None` for an ordinary zero-arg test). A `#[Data([a, b])]` test expands to one
/// `TestCase` per row.
struct TestCase {
    /// The fn to invoke.
    fn_name: String,
    /// The report label (`#[Name]`/fn name, suffixed `[row]` for a data case).
    display: String,
    /// The argument to pass.
    arg: CaseArg,
    /// Where the fn is declared (for the synthesized call's span).
    span: Span,
}

/// A test case's argument: none (an ordinary zero-arg test), a `#[Data]` row value, or an invalid
/// row whose literal cannot become a runtime value (the case fails with this message).
enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Expand a runnable test into its cases: one zero-arg case normally, or one per row when the test
/// carries `#[Data([…])]`. A row literal that cannot be a runtime value (e.g. a bare type name)
/// becomes a case that fails with a clear message rather than being silently dropped.
fn test_cases(test: &TierFn) -> Vec<TestCase> {
    let base = test_display_name(test);
    let Some(rows) = data_rows(test) else {
        return vec![TestCase {
            fn_name: test.name.clone(),
            display: base,
            arg: CaseArg::None,
            span: test.span,
        }];
    };
    rows.iter()
        .map(|row| {
            let arg = match attr_value_to_expr(row, test.span) {
                Some(expr) => CaseArg::Value(expr),
                None => CaseArg::Invalid(format!(
                    "`#[Data]` row `{}` is not a runtime value",
                    attr_value_label(row)
                )),
            };
            TestCase {
                fn_name: test.name.clone(),
                display: format!("{base}[{}]", attr_value_label(row)),
                arg,
                span: test.span,
            }
        })
        .collect()
}

/// Convert a `#[Data]` row literal to an expression to pass as the test argument. Scalars and lists
/// (recursively) are supported; other literal forms (map/set/enum/struct/type-ref) return `None` and
/// surface as a failing case.
fn attr_value_to_expr(value: &AttrValue, span: Span) -> Option<Expr> {
    Some(match value {
        AttrValue::Str(s) => Expr::Str {
            value: s.clone(),
            span,
        },
        AttrValue::Int(n) => Expr::Int { value: *n, span },
        AttrValue::Float(f) => Expr::Float { value: *f, span },
        AttrValue::Bool(b) => Expr::Bool { value: *b, span },
        AttrValue::List(items) => Expr::List {
            items: items
                .iter()
                .map(|item| attr_value_to_expr(item, span))
                .collect::<Option<Vec<_>>>()?,
            span,
        },
        _ => return None,
    })
}

/// A short label for a `#[Data]` row, used in the `name[row]` case display.
fn attr_value_label(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(attr_value_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "?".to_string(),
    }
}

/// The rows of a `#[Data([…])]` attribute on `test`, if present — the elements of its list argument.
fn data_rows(test: &TierFn) -> Option<Vec<AttrValue>> {
    let attr = test
        .attrs
        .iter()
        .find(|a| a.name == lang_ast::reflect::TEST_ATTR_DATA)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::List(items) => Some(items.clone()),
        _ => None,
    })
}

/// Whether a test fn is marked `#[Skip]` — the runner reports it skipped and does not run it.
fn test_is_skipped(test: &TierFn) -> bool {
    test.attrs
        .iter()
        .any(|a| a.name == lang_ast::reflect::TEST_ATTR_SKIP)
}

/// The report label for a skipped test: its display name, plus a `(reason)` when `#[Skip("reason")]`
/// gave one.
fn skip_label(test: &TierFn) -> String {
    let name = test_display_name(test);
    match string_attr(test, lang_ast::reflect::TEST_ATTR_SKIP) {
        Some(reason) if !reason.is_empty() => format!("{name} ({reason})"),
        _ => name,
    }
}

/// A test's display name — the string in `#[Name("…")]` if present, else the fn's own name.
fn test_display_name(test: &TierFn) -> String {
    string_attr(test, lang_ast::reflect::TEST_ATTR_NAME).unwrap_or_else(|| test.name.clone())
}

/// A test's group — the string in `#[Group("…")]` if present, for `--group` filtering.
fn test_group(test: &TierFn) -> Option<String> {
    string_attr(test, lang_ast::reflect::TEST_ATTR_GROUP)
}

/// The first string-valued argument of the attribute named `name` on `test`, if any.
fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

/// Whether a top-level statement is tier-runner *setup* — a declaration or a global binding the
/// tests/benches may depend on — as opposed to the program's own "main" effects (which the
/// `lang test`/`lang bench` runners do not run; they run the tier fns, not the file).
fn is_tier_setup(stmt: &Stmt) -> bool {
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

/// A statement that calls fn `name` with `args`: `name(args…);`. Zero `args` is the ordinary
/// test/bench call; a single arg is a `#[Data]` row.
fn call_stmt(name: &str, args: Vec<Expr>, span: Span) -> Stmt {
    Stmt::Expr {
        expr: Expr::Call {
            callee: Box::new(Expr::Ident {
                name: name.to_string(),
                span,
            }),
            args,
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

/// Run `cases` concurrently across `jobs` worker threads, each grabbing the next case by an atomic
/// index. By default every case runs; with `fail_fast` a failure sets a shared stop flag and the
/// workers drain out. Results are gathered with their original index and returned in declaration
/// order, so the report is deterministic regardless of completion order.
fn run_tests(
    setup: &[Stmt],
    cases: &[TestCase],
    span: Span,
    jobs: usize,
    fail_fast: bool,
) -> Vec<TestOutcome> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results: Mutex<Vec<(usize, TestOutcome)>> = Mutex::new(Vec::with_capacity(cases.len()));

    thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    if fail_fast && stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= cases.len() {
                        break;
                    }
                    let outcome = run_one_test(setup, &cases[idx], span);
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

/// Run a single test case: synthesize `setup + <call the fn (with its data arg, if any)>`, run it in
/// a fresh real-host isolate, and read a nonzero exit / any diagnostic as a failure (the first
/// diagnostic — the assertion or panic — is the reported message). An invalid `#[Data]` row fails
/// without running. The synthesized program is a subset of the already-checked activated program
/// plus one call, so it cannot introduce new type errors; one is surfaced as a failure rather than
/// panicking the worker.
fn run_one_test(setup: &[Stmt], case: &TestCase, span: Span) -> TestOutcome {
    let args = match &case.arg {
        CaseArg::None => Vec::new(),
        CaseArg::Value(expr) => vec![expr.clone()],
        CaseArg::Invalid(message) => {
            return TestOutcome {
                name: case.display.clone(),
                passed: false,
                message: Some(message.clone()),
                stdout: String::new(),
            };
        }
    };
    let display = case.display.clone();
    let mut stmts = setup.to_vec();
    stmts.push(call_stmt(&case.fn_name, args, case.span));
    let program = Program { stmts, span };

    let checked = lang_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return TestOutcome {
            name: display,
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
                name: display,
                passed,
                message,
                stdout: result.stdout,
            }
        }
        Err(err) => TestOutcome {
            name: display,
            passed: false,
            message: Some(err),
            stdout: String::new(),
        },
    }
}

/// Print the per-test report and the summary, returning the process exit code (success only when
/// every selected test ran and passed — a `#[Skip]`ped test does not fail the suite). Failing tests
/// show their message and any captured stdout; skipped tests are listed after. `total` counts every
/// selected test (run + skipped); `outcomes` are the runnable ones (fewer than run on `--fail-fast`).
fn report(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
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
    for name in skipped {
        println!("  skip  {name}");
    }

    let failed = outcomes.len() - passed;
    let not_run = total - skipped.len() - outcomes.len();
    println!();
    let mut parts = vec![format!("{passed} passed"), format!("{failed} failed")];
    if !skipped.is_empty() {
        parts.push(format!("{} skipped", skipped.len()));
    }
    if not_run > 0 {
        parts.push(format!("{not_run} not run (stopped early)"));
    }
    parts.push(format!("{total} total"));
    println!("{}", parts.join(", "));
    let _ = io::stdout().flush();

    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The default number of iterations a benchmark runs when neither the `--iterations` flag nor a
/// per-bench `@bench(iterations: N)` directive sets one. Small, because the runner executes
/// *interpreted* code and measures at both N and 2N (see [`cmd_bench`]); a heavy body lowers it.
const DEFAULT_BENCH_ITERATIONS: u64 = 200;

/// `lang bench <FILE>` — discover the program's `@bench` blocks (object-model slice 6) and measure
/// each. Unlike `lang test`, benchmarks run **sequentially** (concurrency would corrupt timings).
/// Each bench's per-iteration cost is estimated by a **two-point** measurement: the fn is invoked N
/// and 2N times in fresh isolates and the per-iteration time is `(t(2N) − t(N)) / N`, which cancels
/// the fixed per-run overhead (runtime startup, global/setup evaluation, IR lowering — all identical
/// between the two runs). N comes from `--iterations`, else the per-bench `@bench(iterations: N)`
/// directive, else [`DEFAULT_BENCH_ITERATIONS`].
fn cmd_bench(
    file: &std::path::Path,
    iterations_override: Option<u64>,
    profile: &Option<String>,
) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "bench") {
        return code;
    }
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

    // Activate the `bench` tier: inline its `@bench` blocks as ordinary top-level declarations and
    // collect the bench fns (with their directive args). An unknown-tier block is an E0036.
    let activated = lang_check::activate_tiers(&linked.program, &["bench"]);
    if !activated.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, activated.diagnostics.iter());
        return ExitCode::from(1);
    }

    // Type-check once, so a broken benchmark is a compile error reported here rather than inside
    // every per-bench run.
    let checked = lang_check::check_all(&activated.program);
    if !checked.diagnostics.is_empty() {
        emit_diagnostics_mapped(&linked.sources, checked.diagnostics.iter());
        return ExitCode::from(1);
    }

    if activated.benches.is_empty() {
        println!("no benchmarks found");
        return ExitCode::SUCCESS;
    }

    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    let total = activated.benches.len();
    println!("running {total} benchmark{}", plural(total));

    let mut failed = 0usize;
    for bench in &activated.benches {
        let n = iterations_override
            .or_else(|| iterations_arg(&bench.args))
            .unwrap_or(DEFAULT_BENCH_ITERATIONS)
            .max(1);
        match (
            measure_iterations(&setup, bench, n),
            measure_iterations(&setup, bench, n.saturating_mul(2)),
        ) {
            (Ok(t1), Ok(t2)) => {
                let per_ns = ((t2.as_nanos() as f64 - t1.as_nanos() as f64) / n as f64).max(0.0);
                println!(
                    "  {:<28} {:>11}/iter  ({n} iterations)",
                    bench.name,
                    fmt_per_iter(per_ns),
                );
            }
            (Err(msg), _) | (_, Err(msg)) => {
                failed += 1;
                println!("  {:<28} FAILED: {msg}", bench.name);
            }
        }
    }

    println!();
    println!("{} ran, {failed} failed, {total} total", total - failed,);
    let _ = io::stdout().flush();
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The `iterations` argument of a `@bench(...)` directive, if present and positive — the per-bench
/// override of the default iteration count. Resolved through the shared tier-arg schema, so both the
/// positional (`@bench(1000)`) and named (`@bench(iterations: 1000)`) forms work identically.
fn iterations_arg(args: &[AttrArg]) -> Option<u64> {
    match lang_check::bind_tier_args("bench", args)
        .values
        .get("iterations")
    {
        Some(AttrValue::Int(n)) if *n > 0 => Some(*n as u64),
        _ => None,
    }
}

/// Measure executing `bench` `n` times: synthesize `setup + n×<call the bench fn>`, then run it in a
/// fresh real-host isolate, timing **only execution** (IR lowering is done untimed, before the
/// clock starts). A discarded warm-up run plus the minimum of three measured runs damps noise. A
/// nonzero exit / any diagnostic (a panic in the bench body) is a failure, surfaced as `Err`.
fn measure_iterations(
    setup: &[Stmt],
    bench: &TierFn,
    n: u64,
) -> Result<std::time::Duration, String> {
    let mut stmts = setup.to_vec();
    let call = call_stmt(&bench.name, Vec::new(), bench.span);
    stmts.reserve(n as usize);
    for _ in 0..n {
        stmts.push(call.clone());
    }
    let program = Program {
        stmts,
        span: bench.span,
    };

    let checked = lang_check::check_all(&program);
    if !checked.diagnostics.is_empty() {
        return Err(checked.diagnostics[0].message.clone());
    }

    // Take the minimum of three runs: `min` is the standard robust estimator (the fastest run is
    // the one least perturbed by scheduler/GC/OS noise) and inherently discards the cold first run,
    // so no separate warm-up is needed.
    let mut best: Option<std::time::Duration> = None;
    for _ in 0..3 {
        let (result, elapsed) = bench_execute(&program, &checked)?;
        if result.exit_code != 0 || !result.diagnostics.is_empty() {
            return Err(result
                .diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| format!("exited with code {}", result.exit_code)));
        }
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    Ok(best.expect("three measured runs"))
}

/// Lower a program for the real host (untimed) and execute it, returning the result and the
/// **execution-only** wall-clock duration (lowering excluded). Mirrors [`execute_real_host`]'s
/// pipeline so a benchmark runs the same Core-IR path a normal `lang run` does.
fn bench_execute(
    program: &Program,
    checked: &lang_check::Checked,
) -> Result<(lang_backend::RunResult, std::time::Duration), String> {
    let host =
        lang_runtime::RealHost::new().map_err(|err| format!("cannot start the runtime: {err}"))?;
    let relevance = lang_ir_passes::Relevance {
        locals: checked.destructor_relevance.locals.clone(),
        params: checked.destructor_relevance.params.clone(),
    };
    let ir = lang_ir::lower_with_packed(program, &checked.packed_list_sites)
        .expect("Core-IR lowering is total over the parsed language");
    let ir = lang_ir_passes::insert_drops(&ir, Some(&relevance));
    let ir = lang_ir_passes::thread_reuse(&ir);
    let start = Instant::now();
    let result = TreeWalkBackend::new().run_ir_with_host(
        program,
        &ir,
        Box::new(host),
        checked.type_of_sites.clone(),
    );
    Ok((result, start.elapsed()))
}

/// Format a per-iteration duration (in nanoseconds) with an adaptive unit, so a fast op reads in
/// `ns` and a slow one in `ms`/`s`.
fn fmt_per_iter(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

/// `lang doc <FILE>` — extract the program's `@doc { … }` text blocks (object-model slice 6f) to
/// stdout, in source order. Each block's verbatim body is dedented (the common leading indentation
/// and the surrounding blank lines from sitting inside `@doc { … }` are stripped) and preceded by an
/// HTML-comment header noting its source location — valid markdown that renders to nothing. The
/// program is not type-checked or run; doc extraction works on a parse alone, so docs can be pulled
/// from work-in-progress code.
fn cmd_doc(file: &std::path::Path, profile: &Option<String>) -> ExitCode {
    if let Some(code) = profile_gate(file, profile, "doc") {
        return code;
    }
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

    let docs = lang_check::collect_docs(&linked.program);
    if docs.is_empty() {
        eprintln!("lang: no `@doc` blocks found");
        return ExitCode::SUCCESS;
    }

    let mut out = String::new();
    for (i, doc) in docs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let source = linked.sources.source(doc.span.source);
        let line = source.line_col(doc.span.start).line;
        out.push_str(&format!("<!-- {}:{} -->\n", source.name(), line));
        out.push_str(&dedent(&doc.text));
        out.push('\n');
    }
    print!("{out}");
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

/// Dedent a verbatim doc body for presentation: drop leading/trailing blank lines, then strip the
/// common leading whitespace shared by all non-blank lines (so text written indented inside
/// `@doc { … }` renders flush-left). Blank lines do not count toward the common indent and are
/// emitted empty. The lexer captured the body exactly; this is purely the doc generator's
/// formatting, leaving the AST's bytes untouched.
fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    // Trim leading and trailing blank lines.
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(start);
    let body = &lines[start..=end];

    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    body.iter()
        .map(|l| {
            if l.trim().is_empty() {
                ""
            } else {
                &l[indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
