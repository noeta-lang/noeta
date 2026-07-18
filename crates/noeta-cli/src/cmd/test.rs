//! `noeta test` — discover and run a program's `@test` blocks concurrently in fresh
//! isolates, plus the tier-fn attribute helpers the bench runner shares.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use noeta_ast::{AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_span::Span;

use crate::cmd::run::execute_real_host;
use crate::context::{Prologue, check_under, tier_prologue};
use crate::output::plural;

/// The outcome of running one `@test` fn: whether it passed, the failure message (the first
/// diagnostic, typically the assertion/panic), and anything it wrote to stdout (shown on failure).
pub(crate) struct TestOutcome {
    name: String,
    passed: bool,
    message: Option<String>,
    stdout: String,
}

/// `noeta test <FILE>` — discover the program's `@test` blocks (object-model slice 6) and run each
/// as an isolated test. Tests run concurrently (one fresh isolate per test) and, by default, **all**
/// of them run even after a failure; `--fail-fast` stops at the first failure. A test fails when its
/// fn aborts — a false `assert`/`panic` (or any runtime error) — and passes when it returns normally.
/// The program's own top-level "main" effects are not run: `noeta test` runs the tests, not the file.
pub(crate) fn cmd_test(
    file: &std::path::Path,
    fail_fast: bool,
    jobs: Option<usize>,
    group: &Option<String>,
    names: &[String],
    json: bool,
    target: &Option<String>,
) -> ExitCode {
    // The shared tier prologue: compose delegation, the `--target` gate, the dep-aware load,
    // provider dispatch (a `test = "<pkg>"` target hands the tier to that package's runner),
    // activation diagnostics, and the whole-program type check.
    let run = match tier_prologue(file, "test", target) {
        Prologue::Ran(code) => return code,
        Prologue::Ready(run) => *run,
    };
    let activated = &run.activated;

    if activated.tests.is_empty() {
        if json {
            return report_json(&[], &[], 0);
        }
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
    // `--name` (ide-ui U3) then narrows to the named fn(s) — the editor's run-one-test seam.
    let selected: Vec<&TierFn> = match group {
        Some(g) => activated
            .tests
            .iter()
            .filter(|t| test_group(t).as_deref() == Some(g.as_str()))
            .collect(),
        None => activated.tests.iter().collect(),
    };
    let selected: Vec<&TierFn> = if names.is_empty() {
        selected
    } else {
        selected
            .into_iter()
            .filter(|t| names.contains(&t.name))
            .collect()
    };
    if selected.is_empty() {
        if json {
            return report_json(&[], &[], 0);
        }
        match (group, names.is_empty()) {
            (_, false) => println!("no tests matching --name"),
            (Some(g), _) => println!("no tests in group `{g}`"),
            (None, _) => println!("no tests found"),
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
    if !json {
        println!(
            "running {run_count} test{} on {jobs} thread{}{skipped_note}",
            plural(run_count),
            plural(jobs),
        );
    }

    let outcomes = run_tests(
        &setup,
        &run.editions,
        &cases,
        activated.program.span,
        jobs,
        fail_fast,
    );
    if json {
        report_json(&outcomes, &skipped, total)
    } else {
        report(&outcomes, &skipped, total)
    }
}

/// One runnable test invocation: which fn to call, the report label, and an optional argument (a
/// `#[Data]` row — `None` for an ordinary zero-arg test). A `#[Data([a, b])]` test expands to one
/// `TestCase` per row.
pub(crate) struct TestCase {
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
pub(crate) enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Expand a runnable test into its cases: one zero-arg case normally, or one per row when the test
/// carries `#[Data([…])]`. A row literal that cannot be a runtime value (e.g. a bare type name)
/// becomes a case that fails with a clear message rather than being silently dropped.
pub(crate) fn test_cases(test: &TierFn) -> Vec<TestCase> {
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
pub(crate) fn attr_value_to_expr(value: &AttrValue, span: Span) -> Option<Expr> {
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
pub(crate) fn attr_value_label(value: &AttrValue) -> String {
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
pub(crate) fn data_rows(test: &TierFn) -> Option<Vec<AttrValue>> {
    let attr = test
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TEST_ATTR_DATA)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::List(items) => Some(items.clone()),
        _ => None,
    })
}

/// Whether a test fn is marked `#[Skip]` — the runner reports it skipped and does not run it.
pub(crate) fn test_is_skipped(test: &TierFn) -> bool {
    test.attrs
        .iter()
        .any(|a| a.name == noeta_ast::reflect::TEST_ATTR_SKIP)
}

/// The report label for a skipped test: its display name, plus a `(reason)` when `#[Skip("reason")]`
/// gave one.
pub(crate) fn skip_label(test: &TierFn) -> String {
    let name = test_display_name(test);
    match string_attr(test, noeta_ast::reflect::TEST_ATTR_SKIP) {
        Some(reason) if !reason.is_empty() => format!("{name} ({reason})"),
        _ => name,
    }
}

/// A test's display name — the string in `#[Name("…")]` if present, else the fn's own name.
pub(crate) fn test_display_name(test: &TierFn) -> String {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_NAME).unwrap_or_else(|| test.name.clone())
}

/// A test's group — the string in `#[Group("…")]` if present, for `--group` filtering.
pub(crate) fn test_group(test: &TierFn) -> Option<String> {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_GROUP)
}

/// The first string-valued argument of the attribute named `name` on `test`, if any.
pub(crate) fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

/// Whether a top-level statement is tier-runner *setup* — a declaration or a global binding the
/// tests/benches may depend on — as opposed to the program's own "main" effects (which the
/// `noeta test`/`noeta bench` runners do not run; they run the tier fns, not the file).
pub(crate) fn is_tier_setup(stmt: &Stmt) -> bool {
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

/// A statement that calls fn `name` with `args`: `name(args…);`. `name` may be a **qualified**
/// module path (`fuzz.tiers.run_fuzz`, a declared tier's runner) — a dotted identifier the compiler
/// resolves as a qualified reference, so it is emitted verbatim as an `Expr::Ident`, NOT split into
/// a member access. (A method **root** is invoked with [`call_root_stmt`], which does treat the dot
/// as `Type.method`.)
pub(crate) fn call_stmt(name: &str, args: Vec<Expr>, span: Span) -> Stmt {
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

/// A statement that calls a **tier root** by name. A root is either a top-level fn (a bare name) or a
/// method root named `Type.method` — the latter is called as an associated function
/// `Type.method(args…)` (a bare-type-name receiver the compiler resolves to a no-receiver associated
/// call). Unlike [`call_stmt`], the dot here IS a receiver split — a method root's name is exactly
/// `Type.method`, never a deeper module path.
pub(crate) fn call_root_stmt(name: &str, args: Vec<Expr>, span: Span) -> Stmt {
    Stmt::Expr {
        expr: Expr::Call {
            callee: Box::new(root_ref(name, span)),
            args,
            span,
        },
        span,
    }
}

/// A reference to a tier root by name: a bare top-level fn is an [`Expr::Ident`]; a method root
/// `Type.method` is an [`Expr::Member`] on the bare type name (an associated-function reference /
/// no-receiver call the compiler resolves at the site). Used both to call a root and to pass a root
/// as the `run` callable a declared tier's runner invokes.
pub(crate) fn root_ref(name: &str, span: Span) -> Expr {
    match name.split_once('.') {
        Some((type_name, method)) => Expr::Member {
            receiver: Box::new(Expr::Ident {
                name: type_name.to_string(),
                span,
            }),
            name: method.to_string(),
            name_span: span,
            span,
        },
        None => Expr::Ident {
            name: name.to_string(),
            span,
        },
    }
}

/// The default test concurrency — the machine's available parallelism (1 if it cannot be queried).
pub(crate) fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Run `cases` concurrently across `jobs` worker threads, each grabbing the next case by an atomic
/// index. By default every case runs; with `fail_fast` a failure sets a shared stop flag and the
/// workers drain out. Results are gathered with their original index and returned in declaration
/// order, so the report is deterministic regardless of completion order.
pub(crate) fn run_tests(
    setup: &[Stmt],
    editions: &noeta_lexer::EditionMap,
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
                    let outcome = run_one_test(setup, editions, &cases[idx], span);
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
pub(crate) fn run_one_test(
    setup: &[Stmt],
    editions: &noeta_lexer::EditionMap,
    case: &TestCase,
    span: Span,
) -> TestOutcome {
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
    stmts.push(call_root_stmt(&case.fn_name, args, case.span));
    let program = Program { stmts, span };

    let checked = check_under(&program, editions);
    if !checked.diagnostics.is_empty() {
        return TestOutcome {
            name: display,
            passed: false,
            message: Some(checked.diagnostics[0].message.clone()),
            stdout: String::new(),
        };
    }

    // `@test`/`@bench` compile a *separate* module per case (a different granularity than the
    // whole-file startup cache), so they don't participate in it — see `plans/startup-cache`. They
    // have no program pass-through args; a test sees the real process argv.
    match execute_real_host(&program, &checked, std::env::args().collect()) {
        // The `@test` runner reports the failing diagnostic; the trace is a `noeta run` affordance.
        Ok((result, _trace)) => {
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
/// `noeta test --json`: the machine-readable report — one JSON object on stdout with per-test
/// outcomes (name = the report label: fn name / `#[Name]`, `[row]`-suffixed for data cases), the
/// skipped labels, and the totals. The seam the editor's test explorer parses; same exit-code
/// semantics as the human report.
pub(crate) fn report_json(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.len() - passed;
    let not_run = total.saturating_sub(skipped.len() + outcomes.len());
    let json = serde_json::json!({
        "tests": outcomes.iter().map(|o| serde_json::json!({
            "name": o.name,
            "passed": o.passed,
            "message": o.message,
            "stdout": o.stdout,
        })).collect::<Vec<_>>(),
        "skipped": skipped,
        "passed": passed,
        "failed": failed,
        "notRun": not_run,
        "total": total,
    });
    println!("{json}");
    let _ = io::stdout().flush();
    if failed == 0 && not_run == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

pub(crate) fn report(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> ExitCode {
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
