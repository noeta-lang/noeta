//! `noeta test` — discover and run a program's `@test` blocks concurrently in fresh
//! isolates, plus the tier-fn attribute helpers the bench runner shares.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use noeta_ast::{AttrValue, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_span::Span;

use crate::cmd::run::execute_real_host;
use crate::context::{Prologue, check_under, tier_prologue};
use crate::output::{emit_diagnostics_mapped, plural};

/// The per-test deadline in seconds when nothing overrides it — the number a test has to exceed
/// before the runner declares it stuck and moves on.
///
/// **Where it comes from.** Two constraints pin it from opposite sides. From below: it must never
/// fire on a legitimate test. Everything a `@test` body can do that is *slow* rather than *stuck*
/// bottoms out in an I/O client that already carries its own bound — `std.http`'s request timeout,
/// a database driver's connect timeout, a subprocess `wait` — and those bounds are conventionally
/// 30 s or less, so a test that is merely waiting on the world resolves under a minute or fails on
/// its own terms first. Measured against this repo's own corpus the margin is far wider than that:
/// the slowest `@test` in `examples/` runs in tens of milliseconds, i.e. three orders of magnitude
/// under this. From above: it must be short enough that a wedged suite is an inconvenience rather
/// than an outage. The incident that motivated this rail sat **25+ minutes** with no output; a
/// 60 s bound turns that into a named failure in about a fortieth of the time, and a suite of `N`
/// wedged tests into `N`-over-`--jobs` minutes rather than never.
///
/// 60 s is also the period `cargo-nextest` uses before it calls a test slow, so the number will not
/// surprise anyone arriving from Rust.
pub(crate) const DEFAULT_TEST_TIMEOUT_SECS: u64 = 60;

/// How one `@test` case ended. A timeout is deliberately **not** folded into `Failed`: a failing
/// test ran and disagreed with an assertion, a timed-out test did not finish, and the two want
/// different reactions (fix the code vs. raise the bound or find the deadlock). The report, the
/// `--json` seam and the summary counts all keep them apart.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Passed,
    Failed,
    TimedOut,
}

impl Outcome {
    /// The stable `--json` spelling.
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::TimedOut => "timedOut",
        }
    }
}

/// The outcome of running one `@test` fn: how it ended, the failure/timeout message (for a failure,
/// the first diagnostic — typically the assertion or panic), and anything it wrote to stdout (shown
/// on failure).
pub(crate) struct TestOutcome {
    name: String,
    outcome: Outcome,
    message: Option<String>,
    stdout: String,
}

impl TestOutcome {
    fn passed(&self) -> bool {
        self.outcome == Outcome::Passed
    }
}

/// The deadline one case runs under, and *which knob set it* — so the message a timed-out test
/// prints can name the thing the reader has to change.
#[derive(Clone, Copy)]
pub(crate) enum Bound {
    /// No deadline at all: `--timeout 0`, or this test's own `#[Timeout(0)]`.
    None,
    /// The suite-wide default — the built-in [`DEFAULT_TEST_TIMEOUT_SECS`] or `--timeout N`.
    Suite(u64),
    /// This test's own `#[Timeout(N)]`, which wins over the suite default in both directions.
    Attribute(u64),
}

impl Bound {
    /// The suite default `secs` (0 ⇒ no bound), before any per-test attribute.
    fn suite(secs: u64) -> Bound {
        if secs == 0 {
            Bound::None
        } else {
            Bound::Suite(secs)
        }
    }

    /// How long to wait, or `None` when this case is unbounded.
    fn duration(self) -> Option<Duration> {
        match self {
            Bound::None => None,
            Bound::Suite(s) | Bound::Attribute(s) => Some(Duration::from_secs(s)),
        }
    }

    /// What a test that blew this bound should say. It names all three things a reader needs to
    /// act: what the bound was, where it came from, and the exact spelling that raises it — plus,
    /// when the case could not be stopped, what that costs the rest of the run.
    fn timeout_message(self, fn_name: &str, overrun: Overrun) -> String {
        let bound = match self {
            // Unreachable in practice (an unbounded case cannot time out), but a total match beats
            // an `unwrap` in a reporting path.
            Bound::None => "did not finish, and had no deadline to exceed".to_string(),
            Bound::Suite(s) => format!(
                "timed out: did not finish within {s}s (the suite deadline). Raise it for this \
                 test with `#[std.test.Timeout(<seconds>)]` on `{fn_name}`, for the whole run \
                 with `noeta test --timeout <seconds>`, or remove the bound with `--timeout 0`"
            ),
            Bound::Attribute(s) => format!(
                "timed out: did not finish within {s}s (its own `#[Timeout({s})]`). Raise the \
                 number on `{fn_name}`, or write `#[Timeout(0)]` to remove the bound"
            ),
        };
        match overrun {
            // The ordinary ending: the case was asked to stop and did, so the report has nothing
            // extra to warn about and stays as short as it always was.
            Overrun::Stopped => bound,
            // The residual class, and worth a sentence: a reader chasing a slow suite needs to know
            // that this one is still running, and *why* asking did not work — it is the difference
            // between "raise the bound" and "put a deadline on the call this test is stuck in".
            Overrun::Abandoned => format!(
                "{bound}. It was asked to stop and did not, so its thread was abandoned: it keeps \
                 running — holding its isolate, its heap and any open files or sockets — until \
                 this run exits. A test that will not stop is blocked inside a native call (a \
                 socket or pipe read, a subprocess wait) that no safepoint can reach; put the \
                 deadline on that operation rather than on the test around it"
            ),
        }
    }
}

/// `noeta test [PATH]` — discover `@test` blocks (object-model slice 6) and run each as an isolated
/// test. Tests run concurrently (one fresh isolate per test) and, by default, **all** of them run
/// even after a failure; `--fail-fast` stops at the first failure. A test fails when its fn aborts —
/// a false `assert`/`panic` (or any runtime error) — and passes when it returns normally. The
/// program's own top-level "main" effects are not run: `noeta test` runs the tests, not the file.
///
/// `PATH` (default `.`) is a file or a **directory**, mirroring `noeta check`. A directory is walked
/// recursively and every `.noe` file is run as its own entry, because that is the only way a
/// project's tests all run: the linker merges a sibling module's *reachable declarations* into an
/// entry, never its `@test` blocks, so `noeta test src/main.noe` on a two-module project reported
/// "4 passed" while a whole module's tests silently never ran — and a directory argument used to be
/// a raw `Is a directory (os error 21)`, so there was no spelling that did run them.
///
/// The flags arrive as the `TestOpts` the verb already parsed them into, rather than as eight
/// positional parameters. The runner funnelled them into a `TestOptions` on its first line anyway,
/// so the flat list was a third spelling of one record — and every new knob had to be threaded
/// through all three.
pub(crate) fn cmd_test(path: &std::path::Path, flags: &crate::tier_runner::TestOpts) -> u8 {
    let opts = TestOptions {
        fail_fast: flags.fail_fast,
        jobs: flags.jobs,
        group: &flags.group,
        names: &flags.names,
        json: flags.json,
        target: &flags.target,
        timeout: flags.timeout.unwrap_or(DEFAULT_TEST_TIMEOUT_SECS),
    };
    if path.is_dir() {
        return test_directory(path, &opts);
    }
    match run_file_tests(path, &opts, None) {
        FileTests::Ran(code) => code,
        FileTests::None { any_declared } => {
            if flags.json {
                return report_json(&[], &[], 0);
            }
            println!(
                "{}",
                empty_message(any_declared, &flags.group, &flags.names)
            );
            0
        }
        FileTests::Collected {
            outcomes,
            skipped,
            total,
        } => {
            if flags.json {
                report_json(&outcomes, &skipped, total)
            } else {
                report(&outcomes, &skipped, total)
            }
        }
    }
}

/// Everything a test run carries besides the path it runs over — the selection filters, the
/// concurrency, and how to report. Threaded as one value so the file and directory paths take the
/// same options without a wall of parameters.
struct TestOptions<'a> {
    fail_fast: bool,
    jobs: Option<usize>,
    group: &'a Option<String>,
    names: &'a [String],
    json: bool,
    target: &'a Option<String>,
    /// The suite-wide per-test deadline in seconds; `0` disables it. Already defaulted, so the
    /// runner never has to re-derive it.
    timeout: u64,
}

/// Why a file contributed no test outcomes, and what its report should say.
enum FileTests {
    /// The tier prologue short-circuited — compose delegation, a `--target` that does not make the
    /// `test` tier live, or a load/activation/type error already rendered. Carries its exit code.
    Ran(u8),
    /// The file ran, but selected no tests. `any_declared` distinguishes "declares none at all"
    /// from "declares some, and the `--group`/`--name` filters kept none".
    None { any_declared: bool },
    /// The file's selected tests ran.
    Collected {
        outcomes: Vec<TestOutcome>,
        skipped: Vec<String>,
        total: usize,
    },
}

/// The message for a run that selected no tests — the three-way wording the single-file report has
/// always used, kept in one place now that two callers need it.
fn empty_message(any_declared: bool, group: &Option<String>, names: &[String]) -> String {
    match (any_declared, group, names.is_empty()) {
        (true, _, false) => "no tests matching --name".to_string(),
        (true, Some(g), _) => format!("no tests in group `{g}`"),
        _ => "no tests found".to_string(),
    }
}

/// Run every `.noe` file under `dir` as its own entry, aggregating one report and one exit code.
///
/// Each outcome is labelled with the file it came from (`src/human.noe::bytes_stay_bytes`), so a
/// name that appears in two modules stays distinguishable. A file whose prologue fails (a type
/// error, say) is reported through its own already-rendered diagnostics and fails the run, but does
/// not stop the remaining files — a broken module must not hide every other module's results.
fn test_directory(dir: &std::path::Path, opts: &TestOptions) -> u8 {
    let TestOptions {
        fail_fast,
        group,
        names,
        json,
        ..
    } = *opts;
    // Probe compose once for the directory rather than once per file: if this project pins a
    // different toolchain, the delegated run owns the whole directory.
    if crate::compose::maybe_delegate(dir).is_err() {
        // Composition needed but failed (a fixed exit-1 delegation); the tier subsystem is `u8`.
        return 1;
    }
    let files = crate::cmd::check::noe_files(dir);
    let mut outcomes: Vec<TestOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut broken = 0usize;
    // Whether any file declared tests at all — so a filter that matched nothing reports why
    // ("no tests in group `x`") rather than the misleading "no tests found".
    let mut any_declared = false;
    for file in &files {
        let label = file
            .strip_prefix(dir)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned();
        match run_file_tests(file, opts, Some(&label)) {
            FileTests::Ran(code) => {
                if code != 0 {
                    broken += 1;
                }
            }
            FileTests::None { any_declared: d } => any_declared |= d,
            FileTests::Collected {
                outcomes: o,
                skipped: s,
                total: t,
            } => {
                outcomes.extend(o);
                skipped.extend(s);
                total += t;
            }
        }
        if fail_fast && (broken > 0 || outcomes.iter().any(|o| !o.passed())) {
            break;
        }
    }
    if total == 0 && broken == 0 {
        if json {
            return report_json(&[], &[], 0);
        }
        println!("{}", empty_message(any_declared, group, names));
        return 0;
    }
    let code = if json {
        report_json(&outcomes, &skipped, total)
    } else {
        report(&outcomes, &skipped, total)
    };
    if broken > 0 {
        // Otherwise a run that renders a type error and then prints "0 failed" reads as a
        // contradiction against its own nonzero exit.
        eprintln!(
            "noeta: {broken} file{} failed to check; {} tests did not run",
            plural(broken),
            if broken == 1 { "its" } else { "their" }
        );
        return 1;
    }
    code
}

/// Run one file's `@test` blocks. The single-file body of [`cmd_test`], factored out so the
/// directory walk can reuse it; `label`, when given, prefixes every reported test name with the
/// file it came from.
fn run_file_tests(file: &std::path::Path, opts: &TestOptions, label: Option<&str>) -> FileTests {
    let TestOptions {
        fail_fast,
        jobs,
        group,
        names,
        json: quiet,
        target,
        timeout,
    } = *opts;
    // The shared tier prologue: compose delegation, the `--target` gate, the dep-aware load,
    // provider dispatch (a `test = "<pkg>"` target hands the tier to that package's runner),
    // activation diagnostics, and the whole-program type check.
    let run = match tier_prologue(file, "test", target) {
        Prologue::Ran(code) => return FileTests::Ran(code),
        Prologue::Ready(run) => *run,
    };
    let activated = &run.activated;

    if activated.tests.is_empty() {
        return FileTests::None {
            any_declared: false,
        };
    }

    // The setup every test shares: the program's declarations, its top-level bindings/globals, and
    // every top-level effect that *finishes*. Each test then runs as `setup + <call the test fn>` in
    // a fresh isolate, so one test cannot observe another's state.
    //
    // What is left out is decided by `noeta_check::setup`, which asks whether a statement returns
    // rather than what shape it has — so `conn.migrate("migrations")` runs and
    // `server.serve(8080, fetch)` does not. See that module for the policy and its residual holes.
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| noeta_check::is_tier_setup(s, &run.diverging))
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
        return FileTests::None { any_declared: true };
    }

    // Anything the setup left out that a selected test *captures* is reported. The failure this
    // replaces was not the drop itself — it was that a dropped `conn.migrate(…)` and a test
    // asserting on the migrated schema produced a bare assertion failure with nothing pointing at
    // the line that had not run.
    {
        let names: Vec<&str> = selected.iter().map(|t| t.name.as_str()).collect();
        let warnings =
            noeta_check::dropped_setup_warnings(&activated.program, &run.diverging, &names);
        emit_diagnostics_mapped(
            &run.sources,
            warnings
                .iter()
                .map(|w| w.diagnostic())
                .collect::<Vec<_>>()
                .iter(),
        );
    }

    // Partition into skipped (`#[Skip]`) and runnable. A skipped test is reported but never run, and
    // never fails the suite (a skipped `#[Data]` test counts as one skip, not one per row).
    let (skipped_refs, runnable): (Vec<&TierFn>, Vec<&TierFn>) =
        selected.into_iter().partition(|t| test_is_skipped(t));
    let mut skipped: Vec<String> = skipped_refs.iter().map(|t| skip_label(t)).collect();

    // Expand each runnable test into its case(s): a `#[Data([…])]` test runs once per row (reported
    // `name[row]`); an ordinary test is a single zero-arg case.
    let cases: Vec<TestCase> = runnable
        .iter()
        .flat_map(|t| test_cases(t, Bound::suite(timeout)))
        .collect();
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
    if !quiet {
        let in_file = label.map(|l| format!(" in {l}")).unwrap_or_default();
        println!(
            "running {run_count} test{} on {jobs} thread{}{skipped_note}{in_file}",
            plural(run_count),
            plural(jobs),
        );
    }

    let mut outcomes = run_tests(
        &setup,
        &run.opts,
        &cases,
        activated.program.span,
        jobs,
        fail_fast,
    );
    // In a directory run every outcome carries the file it came from, so the same test name in two
    // modules stays distinguishable in one report.
    if let Some(l) = label {
        for outcome in &mut outcomes {
            outcome.name = format!("{l}::{}", outcome.name);
        }
        for name in &mut skipped {
            *name = format!("{l}::{name}");
        }
    }
    FileTests::Collected {
        outcomes,
        skipped,
        total,
    }
}

/// One runnable test invocation: which fn to call, the report label, and an optional argument (a
/// `#[Data]` row — `None` for an ordinary zero-arg test). A `#[Data([a, b])]` test expands to one
/// `TestCase` per row.
#[derive(Clone)]
pub(crate) struct TestCase {
    /// The fn to invoke.
    fn_name: String,
    /// The report label (`#[Name]`/fn name, suffixed `[row]` for a data case).
    display: String,
    /// The argument to pass.
    arg: CaseArg,
    /// Where the fn is declared (for the synthesized call's span).
    span: Span,
    /// Whether the fn is `async fn`, in which case the synthesized call is `.await`ed. Calling one
    /// without awaiting builds a `Future` and drops it, so the body never runs — and a test whose
    /// body never runs *passes*, assertions and all. This flag is the difference between an async
    /// test and a decorative one.
    is_async: bool,
    /// This case's deadline, and which knob set it. A `#[Data]` test's rows each get the fn's
    /// bound — the attribute describes one *test*, and every row is a separate run of it.
    bound: Bound,
}

/// A test case's argument: none (an ordinary zero-arg test), a `#[Data]` row value, or an invalid
/// row whose literal cannot become a runtime value (the case fails with this message).
#[derive(Clone)]
pub(crate) enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Expand a runnable test into its cases: one zero-arg case normally, or one per row when the test
/// carries `#[Data([…])]`. A row literal that cannot be a runtime value (e.g. a bare type name)
/// becomes a case that fails with a clear message rather than being silently dropped.
pub(crate) fn test_cases(test: &TierFn, suite_bound: Bound) -> Vec<TestCase> {
    let base = test_display_name(test);
    let bound = test_bound(test, suite_bound);
    let Some(rows) = data_rows(test) else {
        return vec![TestCase {
            fn_name: test.name.clone(),
            display: base,
            arg: CaseArg::None,
            span: test.span,
            is_async: test.is_async,
            bound,
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
                is_async: test.is_async,
                bound,
            }
        })
        .collect()
}

/// The deadline one test runs under: its own `#[Timeout(N)]` when it carries one, else the suite
/// default. The attribute wins in **both** directions — a test that needs 10 minutes writes
/// `#[Timeout(600)]`, and one that must never take 10 seconds writes `#[Timeout(10)]` — because a
/// bound that could only be raised would make the attribute's number a lie whenever `--timeout`
/// happened to be larger.
pub(crate) fn test_bound(test: &TierFn, suite: Bound) -> Bound {
    match int_attr(test, noeta_ast::reflect::TEST_ATTR_TIMEOUT) {
        // `#[Timeout(0)]` is the local escape hatch: this one test is not bounded at all.
        Some(0) => Bound::None,
        // A negative literal is not a duration; ignore it rather than saturating it to a bound the
        // author plainly did not ask for, and let the suite default keep the rail in place.
        Some(secs) if secs > 0 => Bound::Attribute(secs as u64),
        _ => suite,
    }
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

/// The first int-valued argument of the attribute named `name` on `test`, if any — the shape
/// `#[Timeout(30)]` parses to.
pub(crate) fn int_attr(test: &TierFn, name: &str) -> Option<i64> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Int(n) => Some(*n),
        _ => None,
    })
}

/// The first string-valued argument of the attribute named `name` on `test`, if any.
pub(crate) fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
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
                name: noeta_ast::Name::canonical(name),
                span,
            }),
            args: args
                .into_iter()
                .map(noeta_ast::CallArg::positional)
                .collect(),
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
            args: args
                .into_iter()
                .map(noeta_ast::CallArg::positional)
                .collect(),
            span,
        },
        span,
    }
}

/// A statement that calls a tier root, awaiting it when the root is `async fn`. A call to an async
/// fn evaluates to a `Future`; in statement position that future is constructed and dropped, so the
/// body never runs. For a `@test` root that is silent and total — every assertion in the body passes
/// because none of them executes — so the `.await` here is what makes an async test a test.
///
/// Top-level `.await` is the ordinary spelling (the synthesized program's statements are top level),
/// so this needs nothing from the backends beyond what `expr.await` already does.
pub(crate) fn call_root_stmt_awaited(
    name: &str,
    args: Vec<Expr>,
    span: Span,
    is_async: bool,
) -> Stmt {
    if !is_async {
        return call_root_stmt(name, args, span);
    }
    Stmt::Expr {
        expr: Expr::Await {
            expr: Box::new(Expr::Call {
                callee: Box::new(root_ref(name, span)),
                args: args
                    .into_iter()
                    .map(noeta_ast::CallArg::positional)
                    .collect(),
                span,
            }),
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
                name: noeta_ast::Name::canonical(type_name),
                span,
            }),
            name: method.to_string(),
            name_span: span,
            span,
        },
        None => Expr::Ident {
            name: noeta_ast::Name::canonical(name),
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
///
/// Each case is bounded by its own deadline (see [`run_one_test_bounded`]) — the pool itself is
/// unchanged, but no single case can hold a worker past its bound.
pub(crate) fn run_tests(
    setup: &[Stmt],
    opts: &noeta_check::CheckOptions,
    cases: &[TestCase],
    span: Span,
    jobs: usize,
    fail_fast: bool,
) -> Vec<TestOutcome> {
    // The bounded runner hands each case to a **detached** thread, which needs `'static` data — so
    // the setup and the check options are shared by `Arc` rather than borrowed off the stack. One
    // clone per run, not per case.
    let setup = Arc::new(setup.to_vec());
    let opts = Arc::new(opts.clone());
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
                    let outcome = run_one_test_bounded(&setup, &opts, &cases[idx], span);
                    // A timeout stops a `--fail-fast` run for the same reason a failure does: the
                    // suite is not going to get greener, and the point of `--fail-fast` is to stop
                    // burning wall time.
                    let ended_badly = !outcome.passed();
                    results.lock().unwrap().push((idx, outcome));
                    if fail_fast && ended_badly {
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

/// How long a cancelled case is given to actually stop before the runner gives up on it and
/// abandons its thread — the grace period in [`stop_overrun_case`].
///
/// It is a *grace*, not a second deadline. A case that observes the request at a safepoint unwinds
/// immediately; what this number actually has to cover is the **teardown behind the unwind** —
/// every live destructor, the final cycle collection, joining any isolates the case spawned — on a
/// box already running the rest of the suite. Measured end to end (the moment the deadline expires
/// to the moment the thread is joined): ~50 ms for an ordinary wedged case, and ~165 ms for one
/// holding a 400 000-object heap at the moment it was asked. One second leaves roughly 6× headroom
/// over that and is invisible next to the smallest bound anyone would set.
///
/// The trade runs both ways, which is why it is generous rather than tight: overshooting costs wall
/// time on a run that has to abandon (the grace is waited out per worker, so it is paid once in
/// parallel, not once per case — measured 2.07 s for a suite with seven wedged cases under a 1 s
/// bound), while undershooting silently converts a clean stop into an abandonment and its leak.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// What became of an overrunning case after the runner asked it to stop — the two genuinely
/// different endings, kept apart because they cost the run different things.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overrun {
    /// The case observed the request at a safepoint, unwound, and tore its isolate down. Its thread
    /// is joined and nothing is left behind.
    Stopped,
    /// The case did not stop within [`CANCEL_GRACE`], so its thread was abandoned and everything
    /// its isolate owns is held until the process exits. In practice this means it is blocked
    /// inside a native call, which no safepoint can reach.
    Abandoned,
}

/// Run one case under its deadline, returning a `TimedOut` outcome if it does not finish in time.
///
/// # The shape of the rail
///
/// Two questions, kept apart on purpose:
///
/// 1. **When has this case exceeded its bound, and what does the report say?** That is this
///    function: the case runs on its own thread, the worker waits on a channel with a deadline, and
///    on expiry it produces a `TimedOut` outcome naming the test, the bound and how to raise it.
///    Every other case still runs, the report still prints, and the run still exits.
/// 2. **What happens to the overrunning case itself?** That is [`stop_overrun_case`] — the one
///    place that knows how a running test is actually stopped.
///
/// Keeping (1) separate is what makes the rail complete rather than best-effort: a test blocked
/// inside a native call reaches no safepoint and cannot be stopped by anything, and for that class
/// "stop waiting, report, and move on" is the whole answer rather than a stopgap.
///
/// The case's run is armed with a **cancellation flag** it polls at its own safepoints; the runner
/// stores through it once the deadline expires. A cancelled case still sends its (meaningless)
/// outcome down the same channel, which is how the runner learns it stopped — the value is
/// discarded, since the case was already reported as timed out.
fn run_one_test_bounded(
    setup: &Arc<Vec<Stmt>>,
    opts: &Arc<noeta_check::CheckOptions>,
    case: &TestCase,
    span: Span,
) -> TestOutcome {
    let Some(deadline) = case.bound.duration() else {
        // Unbounded (`--timeout 0` / `#[Timeout(0)]`): run it right here, exactly as before this
        // rail existed. No detached thread, no channel, nothing to cancel and nothing to leak.
        return run_one_test(setup, opts, case, span, None);
    };
    let (tx, rx) = mpsc::channel();
    // The case's stop request. Armed before the run starts, so a deadline that expires while the
    // case is still compiling is honored at the body's very first safepoint.
    let cancel: noeta_vm::CancelFlag = Arc::new(AtomicBool::new(false));
    let spawned = {
        let (setup, opts, owned) = (Arc::clone(setup), Arc::clone(opts), case.clone());
        let cancel = Arc::clone(&cancel);
        // Name the thread after the case: an abandoned thread is visible in a debugger and in
        // `/proc`, and "which test is still spinning" should not need guessing.
        thread::Builder::new()
            .name(format!("noeta-test:{}", case.display))
            .spawn(move || {
                // The receiver is gone once the worker gives up; a failed send is the expected shape
                // of "this test finished after we stopped listening", not an error.
                let _ = tx.send(run_one_test(&setup, &opts, &owned, span, Some(cancel)));
            })
    };
    let handle = match spawned {
        Ok(handle) => handle,
        // The OS refused a thread (an `RLIMIT_NPROC`/memory ceiling under heavy parallelism). Run
        // the case inline rather than reporting a test failure the test did not cause; it is
        // unbounded, which is what the runner did for every test before this rail.
        Err(_) => return run_one_test(setup, opts, case, span, None),
    };
    match rx.recv_timeout(deadline) {
        Ok(outcome) => {
            // The case answered, so its thread has already returned; the join is immediate and
            // reaps it rather than leaving a zombie behind.
            let _ = handle.join();
            outcome
        }
        Err(RecvTimeoutError::Timeout) => {
            let overrun = stop_overrun_case(handle, &rx, &cancel);
            TestOutcome {
                name: case.display.clone(),
                outcome: Outcome::TimedOut,
                message: Some(case.bound.timeout_message(&case.fn_name, overrun)),
                // A stopped case tore its isolate down without producing a result, and an abandoned
                // one is still running — either way there is nothing complete to read. (Even a
                // finished isolate's buffers do not currently reach the parent's `RunResult` — a
                // known gap in `run_isolate_worker`.) Reporting an empty stdout is honest;
                // reporting a partial one would not be.
                stdout: String::new(),
            }
        }
        // The sender was dropped without sending: the case's thread panicked (a bug in the
        // toolchain, not in the test). Before this rail that panic unwound through `thread::scope`
        // and took the whole run with it; now it is one failing test with the reason named.
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            TestOutcome {
                name: case.display.clone(),
                outcome: Outcome::Failed,
                message: Some(format!(
                    "the test runner panicked while running `{}` (see stderr)",
                    case.fn_name
                )),
                stdout: String::new(),
            }
        }
    }
}

/// Deal with a case that blew its deadline — **the one seam** between "the runner decided this test
/// is over" and how the runtime actually stops it: **ask, join with a grace period, abandon only if
/// that expires.**
///
/// `cancel` is the flag the case's own run polls at its safepoints (the dispatch loop's frame
/// transfers and taken loop back-edges, plus each scheduler round). Storing through it is a
/// *request*; the case's arrival on `done` is the *report*, exactly the `cancel`/`join` split the
/// language gives a task handle. `JoinHandle` has no timed join, so the grace is waited out on the
/// result channel the case sends down when its run returns — receiving anything at all means the
/// thread is about to end, and the join behind it is then immediate.
///
/// **The two classes, and why they end differently.**
///
/// - A case executing Noeta — the compute-bound `while true`, the runaway recursion — reaches a
///   safepoint within an iteration, unwinds, and runs the ordinary teardown: its destructors fire,
///   its heap returns to zero residency, and any isolates it spawned are cancelled and joined
///   (`Vm::teardown`). Joining it here is what turns the old leak into no leak at all, and it is
///   also the only ending under which the case's own resources are released rather than merely
///   forgotten.
/// - A case blocked **inside a native call** — a socket read, a pipe read with no writer — is not
///   executing Noeta, so no safepoint comes around and the request cannot land. Nothing available
///   in Rust can stop that thread from outside: `pthread_cancel` against a thread holding an
///   allocator or runtime lock turns a hung test into a hung process. Abandoning it is the only
///   option the rail has, and it is still the right one — a leaked thread that lets the suite finish
///   and *name* the culprit beats a tidy suite that never returns.
///
/// **What abandoning leaks, precisely.** One OS thread and everything its isolate owns: the VM, its
/// heap, its `tokio` runtime, and any descriptors or sockets it holds, until the process exits.
///
/// **Why abandoning is safe here, when abandoning a worker isolate was not.** A worker isolate
/// borrows its arguments zero-copy out of the parent's shared region, so an abandoned worker races
/// the parent's teardown freeing that region — measured as a reproducible segfault in the allocator,
/// which is why a `concurrent` block waits for its members instead. A `@test` case shares no such
/// graph: it is a whole program on its own thread with its own `Host`, its own executor and its own
/// thread-local heap (`noeta-value`'s registry is per-thread — shared-nothing per isolate by
/// construction), and *nothing in the runner ever frees it*. It is leaked, not freed-then-used, and
/// a leak is not a race. Process exit is the same story: mimalloc's `mi_process_done` runs with
/// `destroy_on_exit` off, so it collects the exiting thread's own heap and leaves a live thread's
/// pages alone. Measured over 240 runs of the hostile shapes — a spinning case, six cases allocating
/// flat out, and a case blocked in a FIFO read, alone and together — every run exited `1` cleanly and
/// none died on a signal.
fn stop_overrun_case(
    handle: thread::JoinHandle<()>,
    done: &mpsc::Receiver<TestOutcome>,
    cancel: &noeta_vm::CancelFlag,
) -> Overrun {
    // Ask. Relaxed is enough: the flag only ever goes `false → true` and the case's reaction —
    // unwinding its own frames — is entirely local to its thread.
    cancel.store(true, Ordering::Relaxed);
    match done.recv_timeout(CANCEL_GRACE) {
        // It stopped when asked (or, harmlessly, finished on its own in the same instant). Join it:
        // the send is the last thing its closure does, so this returns at once.
        Ok(_) | Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Overrun::Stopped
        }
        // The grace expired — the case is blocked somewhere no safepoint can reach. Dropping the
        // handle detaches the thread; the join is precisely what a wedged case never returns from.
        Err(RecvTimeoutError::Timeout) => {
            drop(handle);
            Overrun::Abandoned
        }
    }
}

/// Run a single test case: synthesize `setup + <call the fn (with its data arg, if any)>`, run it in
/// a fresh real-host isolate, and read a nonzero exit / any diagnostic as a failure (the first
/// diagnostic — the assertion or panic — is the reported message). An invalid `#[Data]` row fails
/// without running. The synthesized program is a subset of the already-checked activated program
/// plus one call, so it cannot introduce new type errors; one is surfaced as a failure rather than
/// panicking the worker.
///
/// `cancel`, when given, arms the case's run with the cooperative stop request the timeout rail
/// stores through ([`stop_overrun_case`]). A cancelled run returns a `TestOutcome` describing a body
/// that never finished — the rail discards it, having already reported the case as timed out — so
/// this is `None` on every path that actually reads the outcome.
pub(crate) fn run_one_test(
    setup: &[Stmt],
    opts: &noeta_check::CheckOptions,
    case: &TestCase,
    span: Span,
    cancel: Option<noeta_vm::CancelFlag>,
) -> TestOutcome {
    let args = match &case.arg {
        CaseArg::None => Vec::new(),
        CaseArg::Value(expr) => vec![expr.clone()],
        CaseArg::Invalid(message) => {
            return TestOutcome {
                name: case.display.clone(),
                outcome: Outcome::Failed,
                message: Some(message.clone()),
                stdout: String::new(),
            };
        }
    };
    let display = case.display.clone();
    let mut stmts = setup.to_vec();
    stmts.push(call_root_stmt_awaited(
        &case.fn_name,
        args,
        case.span,
        case.is_async,
    ));
    let program = Program { stmts, span };

    let checked = check_under(&program, opts);
    // Only an error fails the case. Warnings are not repeated here: this per-case program is the
    // *same* source the prologue already checked and reported, so re-emitting would print every
    // file-level warning once per test.
    if let Some(error) = checked.diagnostics.iter().find(|d| d.is_error()) {
        return TestOutcome {
            name: display,
            outcome: Outcome::Failed,
            message: Some(error.message.clone()),
            stdout: String::new(),
        };
    }

    // `@test`/`@bench` compile a *separate* module per case (a different granularity than the
    // whole-file startup cache), so they don't participate in it — see `plans/startup-cache`. They
    // have no program pass-through args; a test sees the real process argv.
    match execute_real_host(&program, &checked, std::env::args().collect(), false, cancel) {
        // The `@test` runner reports the failing diagnostic; the trace is a `noeta run` affordance.
        Ok((result, _trace)) => {
            // An abort fails the case; an advisory diagnostic does not.
            let passed =
                result.exit_code == 0 && !noeta_diagnostics::has_errors(&result.diagnostics);
            let message = (!passed).then(|| {
                result
                    .diagnostics
                    .iter()
                    .find(|d| d.is_error())
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| format!("exited with code {}", result.exit_code))
            });
            TestOutcome {
                name: display,
                outcome: if passed {
                    Outcome::Passed
                } else {
                    Outcome::Failed
                },
                message,
                stdout: result.stdout,
            }
        }
        // A synthesized per-case program has no `SourceMap` here, so this stays the one-line
        // rendering — which now at least names the construct precisely.
        Err(u) => TestOutcome {
            name: display,
            outcome: Outcome::Failed,
            message: Some(u.to_string()),
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
pub(crate) fn report_json(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> u8 {
    let tally = Tally::of(outcomes, skipped.len(), total);
    let json = serde_json::json!({
        "tests": outcomes.iter().map(|o| serde_json::json!({
            "name": o.name,
            // The precise outcome — `"passed"` / `"failed"` / `"timedOut"`. The boolean beside it
            // is the older seam and stays: a timed-out test is not passing, so a consumer that only
            // knows `passed` still colors it red rather than silently green.
            "outcome": o.outcome.as_str(),
            "passed": o.passed(),
            "message": o.message,
            "stdout": o.stdout,
        })).collect::<Vec<_>>(),
        "skipped": skipped,
        "passed": tally.passed,
        // `failed` counts assertion/abort failures only; a test that never finished is counted
        // under `timedOut`, because the two ask for different reactions.
        "failed": tally.failed,
        "timedOut": tally.timed_out,
        "notRun": tally.not_run,
        "total": total,
    });
    println!("{json}");
    let _ = io::stdout().flush();
    tally.exit_code()
}

/// The summary counts both reporters derive, in one place so the human table and the JSON can never
/// disagree about how many tests passed or what the exit code should be.
struct Tally {
    passed: usize,
    failed: usize,
    timed_out: usize,
    not_run: usize,
}

impl Tally {
    fn of(outcomes: &[TestOutcome], skipped: usize, total: usize) -> Tally {
        let count = |want: Outcome| outcomes.iter().filter(|o| o.outcome == want).count();
        Tally {
            passed: count(Outcome::Passed),
            failed: count(Outcome::Failed),
            timed_out: count(Outcome::TimedOut),
            not_run: total.saturating_sub(skipped + outcomes.len()),
        }
    }

    /// `0` only when every selected test ran and passed. A timeout fails the suite: "we do not know
    /// whether this test passes" is not a green result.
    fn exit_code(&self) -> u8 {
        if self.failed == 0 && self.timed_out == 0 && self.not_run == 0 {
            0
        } else {
            1
        }
    }
}

pub(crate) fn report(outcomes: &[TestOutcome], skipped: &[String], total: usize) -> u8 {
    for outcome in outcomes {
        // The status column stays four characters wide, so `TIME` is the marker and the line under
        // it carries the whole story (what the bound was, and how to raise it).
        let status = match outcome.outcome {
            Outcome::Passed => "ok  ",
            Outcome::Failed => "FAIL",
            Outcome::TimedOut => "TIME",
        };
        println!("  {status}  {}", outcome.name);
        if outcome.passed() {
            continue;
        }
        if let Some(message) = &outcome.message {
            println!("        {message}");
        }
        for line in outcome.stdout.lines() {
            println!("        | {line}");
        }
    }
    for name in skipped {
        println!("  skip  {name}");
    }

    let tally = Tally::of(outcomes, skipped.len(), total);
    println!();
    let mut parts = vec![
        format!("{} passed", tally.passed),
        format!("{} failed", tally.failed),
    ];
    // Only mentioned when it happened — a suite that never hangs should not have to read a "0 timed
    // out" on every green run.
    if tally.timed_out > 0 {
        parts.push(format!("{} timed out", tally.timed_out));
    }
    if !skipped.is_empty() {
        parts.push(format!("{} skipped", skipped.len()));
    }
    if tally.not_run > 0 {
        parts.push(format!("{} not run (stopped early)", tally.not_run));
    }
    parts.push(format!("{total} total"));
    println!("{}", parts.join(", "));
    let _ = io::stdout().flush();

    tally.exit_code()
}
