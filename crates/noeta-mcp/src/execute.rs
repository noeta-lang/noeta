//! M4 Execute pillar: `run`, `eval`, and `test` — the "run this and tell me what actually happened"
//! loop. Every execution is **sandboxed by default** (deterministic: in-memory `fs`, logical clock,
//! seeded `random`, pure network responders) with a **real-host opt-in** (`real: true`, for "does
//! this actually work end-to-end"), and **always-on liveness limits** bound every run — sandbox or
//! real (decision #5): determinism does not prevent an infinite loop or an output flood.
//!
//! The liveness bound rides the VM's own per-instruction [`Debugger`] seam (the same hook `noeta dap`
//! uses): a [`LimitDebugger`] counts instructions and checks a wall-clock deadline between them,
//! returning [`DebugAction::Terminate`] to abandon the run *inside the VM* — a real bound with clean
//! teardown, no leaked worker thread. `run` compiles through the salsa `linked_bytecode` query and
//! runs the module tier-0; `eval` and `test` ride [`VmSession`], the REPL engine.

use crate::analyze::Prepared;
use noeta_ast::{AttrValue, Attribute, Expr, Program, Stmt};
use noeta_check::TierFn;
use noeta_diagnostics::{JsonDiagnostic, to_json};
use noeta_parser::parse_fragment;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::{DebugAction, DebugView, Debugger, SessionOutput, VmBackend, VmSession};
use rmcp::{ErrorData, schemars};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default wall-clock budget for a `run`, in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Default instruction budget for a `run` — high enough for any real workload, low enough that a
/// runaway loop stops in well under a second of CPU.
const DEFAULT_MAX_STEPS: u64 = 200_000_000;
/// Default cap on returned stdout, in bytes (output past it is dropped, and the caller is told).
const DEFAULT_OUTPUT_BYTES: usize = 64 * 1024;
/// How often (in instructions) the limit hook consults the wall clock — reading `Instant::now()`
/// every op would dominate tier-0 cost, so it is sampled.
const CLOCK_CHECK_INTERVAL: u64 = 4_096;

// Which liveness limit stopped a run (stored through an `Arc<AtomicU8>` the hook writes and the
// caller reads after the debugger is consumed by the run).
const TRIP_NONE: u8 = 0;
const TRIP_STEPS: u8 = 1;
const TRIP_TIMEOUT: u8 = 2;

/// The always-on liveness limits for a `run` (decision #5). Every field defaults; an agent tunes one
/// (e.g. a longer `timeout_ms` for a heavier program) without restating the rest.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct RunLimits {
    /// Wall-clock budget in milliseconds (default 5000), checked between VM instructions. Bounds a
    /// tight loop; a program blocked inside one long native call is bounded only by that call.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Max VM instructions to execute (default 200_000_000). A hard bound independent of the clock.
    #[serde(default)]
    pub max_steps: Option<u64>,
    /// Max stdout bytes to return (default 65536); output beyond it is dropped and flagged.
    #[serde(default)]
    pub output_bytes: Option<usize>,
}

/// The `run` result: what the program wrote, how it exited, and whether a limit cut it short.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RunOutput {
    /// True when the program ran to completion with exit code 0 and no runtime diagnostics.
    pub ok: bool,
    /// True when the program compiled and actually ran. False ⇒ it did not type-check; see
    /// `diagnostics` — nothing was executed.
    pub ran: bool,
    /// The host the program ran against: `"sandbox"` (deterministic) or `"real"`.
    pub host: String,
    /// Everything the program wrote to stdout (truncated to the output cap; see `stdout_truncated`).
    pub stdout: String,
    /// True when `stdout` was truncated at the output-byte cap.
    pub stdout_truncated: bool,
    /// The process exit code (0 for a clean run; nonzero for an aborted or `exit`-ed one).
    pub exit_code: i32,
    /// Compile-time and runtime diagnostics, resolved to file + line/column — the same
    /// `JsonDiagnostic` shape `check` returns. A runtime panic/assert appears here.
    pub diagnostics: Vec<JsonDiagnostic>,
    /// The rendered abort traceback (innermost frame first), when the run aborted with a call chain.
    pub traceback: Option<String>,
    /// Set when a liveness limit stopped the run: `"timeout"` or `"step_limit"`.
    pub limit_hit: Option<String>,
    /// Approximately how many VM instructions executed (sampled).
    pub steps: u64,
}

/// A [`Debugger`] that never pauses — it only enforces the liveness limits (decision #5). Consulted
/// before every tier-0 instruction, it counts steps and, periodically, checks the deadline; when
/// either bound is crossed it records which and asks the VM to [`DebugAction::Terminate`]. The
/// concrete debugger is consumed by the run, so its outcome (`tripped`, `steps`) is shared out
/// through `Arc` atomics the caller keeps.
struct LimitDebugger {
    steps: u64,
    max_steps: u64,
    deadline: Instant,
    tripped: Arc<AtomicU8>,
    step_count: Arc<AtomicU64>,
}

impl Debugger for LimitDebugger {
    fn before_op(&mut self, _proto: u32, _pc: usize, _view: &DebugView) -> DebugAction {
        self.steps += 1;
        if self.steps > self.max_steps {
            self.tripped.store(TRIP_STEPS, Ordering::Relaxed);
            self.step_count.store(self.steps, Ordering::Relaxed);
            return DebugAction::Terminate;
        }
        if self.steps.is_multiple_of(CLOCK_CHECK_INTERVAL) {
            self.step_count.store(self.steps, Ordering::Relaxed);
            if Instant::now() >= self.deadline {
                self.tripped.store(TRIP_TIMEOUT, Ordering::Relaxed);
                return DebugAction::Terminate;
            }
        }
        DebugAction::Continue
    }
}

/// Run a checked program and report what happened. Gates on the type check first (a program with
/// errors never runs); then compiles via the salsa `linked_bytecode` query and executes the module
/// tier-0 against the sandbox (default) or real host, under the always-on liveness limits.
pub fn run(
    p: &Prepared,
    args: Vec<String>,
    real: bool,
    limits: &RunLimits,
) -> Result<RunOutput, ErrorData> {
    let source_map = SourceMap::new(p.sources.clone());

    // Gate: a program that does not type-check never runs — surface the diagnostics and stop. The
    // compiler emits no warnings today, so any diagnostic is a blocking error (see `check`).
    let checked = noeta_db::linked_checked(&p.db, p.ws);
    if !checked.diagnostics.is_empty() {
        return Ok(RunOutput {
            ok: false,
            ran: false,
            host: host_label(real),
            stdout: String::new(),
            stdout_truncated: false,
            exit_code: 0,
            diagnostics: map_diagnostics(&source_map, &checked.diagnostics),
            traceback: None,
            limit_hit: None,
            steps: 0,
        });
    }

    // The compiled module comes off the same memoized query the `bytecode` tool reads (cooperative
    // isolates, no debug info) — a checked program always compiles, so an `Err` is an internal
    // invariant break, not an ordinary user error.
    let compiled = noeta_db::linked_bytecode(&p.db, p.ws);
    let module = match &compiled.0 {
        Ok(module) => module,
        Err(unsupported) => {
            return Err(ErrorData::internal_error(
                format!(
                    "the VM cannot compile this checked program: {}",
                    unsupported.reason
                ),
                None,
            ));
        }
    };

    let (host, executor) = make_host(real, args).map_err(|e| ErrorData::internal_error(e, None))?;
    let tripped = Arc::new(AtomicU8::new(TRIP_NONE));
    let step_count = Arc::new(AtomicU64::new(0));
    let debugger = LimitDebugger {
        steps: 0,
        max_steps: limits.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
        deadline: Instant::now()
            + Duration::from_millis(limits.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
        tripped: tripped.clone(),
        step_count: step_count.clone(),
    };

    let (result, trace) =
        VmBackend::new().run_module_debug(module, host, executor, Some(Box::new(debugger)));

    let cap = limits.output_bytes.unwrap_or(DEFAULT_OUTPUT_BYTES);
    let (stdout, stdout_truncated) = truncate_utf8(result.stdout, cap);
    let limit_hit = match tripped.load(Ordering::Relaxed) {
        TRIP_STEPS => Some("step_limit".to_string()),
        TRIP_TIMEOUT => Some("timeout".to_string()),
        _ => None,
    };
    let traceback = (trace.len() >= 2).then(|| noeta_vm::render_trace(&trace, &source_map));
    let diagnostics = map_diagnostics(&source_map, &result.diagnostics);

    Ok(RunOutput {
        ok: limit_hit.is_none() && result.exit_code == 0 && diagnostics.is_empty(),
        ran: true,
        host: host_label(real),
        stdout,
        stdout_truncated,
        exit_code: result.exit_code,
        diagnostics,
        traceback,
        limit_hit,
        steps: step_count.load(Ordering::Relaxed),
    })
}

/// The `eval` result: the trailing expression's value + type (REPL-style), plus anything it printed.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EvalOutput {
    /// True when the fragment evaluated without a diagnostic.
    pub ok: bool,
    /// The display form of the trailing bare expression's non-unit value (`1 + 2` → `"3"`); `None`
    /// when the fragment ends in a statement or its value is unit.
    pub value: Option<String>,
    /// The value's surface-syntax type (`"int"`, `"List<int>"`), when a value was produced.
    pub r#type: Option<String>,
    /// Anything the fragment (or its `context`) wrote to stdout.
    pub stdout: String,
    /// Diagnostics from parsing or running the fragment (a parse error, an unknown name, a panic).
    pub diagnostics: Vec<JsonDiagnostic>,
}

/// Evaluate one expression against an optional `context` (prior bindings/definitions), REPL-style,
/// via [`VmSession`]. Sandbox by default; `real: true` runs it against the real host. The `context`
/// runs first as its own session entry, then the expression; a value-producing expression is
/// additionally re-typed (as the REPL's `:type` does — so a side-effecting expression runs twice).
pub fn eval(expr: &str, context: Option<&str>, real: bool) -> EvalOutput {
    let mut session = VmSession::new(session_factory(real));
    let ctx_map = SourceMap::new(vec![Source::new(
        SourceId::FIRST,
        "<eval>".to_string(),
        String::new(),
    )]);
    let mut stdout = String::new();

    // Run the context (bindings, fn/type definitions) as a first entry; its diagnostics are fatal to
    // the eval (the expression would reference names that never bound).
    if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
        let frag = parse_fragment(SourceId::FIRST, "<context>", ctx);
        if !frag.diagnostics.is_empty() {
            return EvalOutput {
                ok: false,
                value: None,
                r#type: None,
                stdout,
                diagnostics: map_diagnostics(&ctx_map, &frag.diagnostics),
            };
        }
        let out = session.eval(&frag.program);
        stdout.push_str(&out.stdout);
        if !out.diagnostics.is_empty() {
            return EvalOutput {
                ok: false,
                value: None,
                r#type: None,
                stdout,
                diagnostics: map_diagnostics(&ctx_map, &out.diagnostics),
            };
        }
    }

    let frag = parse_fragment(SourceId::FIRST, "<eval>", expr);
    if !frag.diagnostics.is_empty() {
        return EvalOutput {
            ok: false,
            value: None,
            r#type: None,
            stdout,
            diagnostics: map_diagnostics(&ctx_map, &frag.diagnostics),
        };
    }
    let out = session.eval(&frag.program);
    stdout.push_str(&out.stdout);
    let diagnostics = map_diagnostics(&ctx_map, &out.diagnostics);
    // Type the value only when the fragment produced one (a trailing bare expression). Re-running a
    // definition/binding entry to type it would redefine it; a value-yielding expression is safe to
    // re-run (matching the REPL's `:type`).
    let r#type = (out.value.is_some() && diagnostics.is_empty())
        .then(|| session.type_of(&frag.program).value)
        .flatten();

    EvalOutput {
        ok: diagnostics.is_empty(),
        value: out.value,
        r#type,
        stdout,
        diagnostics,
    }
}

/// The `test` result: one entry per case, plus the roll-up counts.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TestOutput {
    /// True when every runnable case passed (a compile error or a failing case makes this false).
    pub ok: bool,
    /// A compile error that stopped the whole suite before any case ran (nothing in `cases` then).
    pub diagnostics: Vec<JsonDiagnostic>,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Every case, in source order.
    pub cases: Vec<TestCaseResult>,
}

/// One `@test` case's outcome.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TestCaseResult {
    /// The report label — `#[Name(...)]` or the fn name, suffixed `[row]` for a `#[Data]` case.
    pub name: String,
    /// `"pass"`, `"fail"`, or `"skip"`.
    pub status: String,
    /// The failure message (the assertion/panic, or a nonzero exit), when failed.
    pub message: Option<String>,
    /// Anything the case printed (useful on a failure).
    pub stdout: String,
}

/// Run a file's `@test` blocks and report each case. Activates the `test` tier over the linked
/// program, type-checks it once, then runs each case (`setup` + a call to the test fn) as a fresh
/// [`VmSession`] entry — sandbox by default, `real: true` for the real host. `filter` keeps only
/// cases whose test name or `#[Group(...)]` contains it.
pub fn test(p: &Prepared, filter: Option<&str>, real: bool) -> TestOutput {
    let source_map = SourceMap::new(p.sources.clone());
    let empty = |diagnostics| TestOutput {
        ok: false,
        diagnostics,
        passed: 0,
        failed: 0,
        skipped: 0,
        cases: Vec::new(),
    };

    // The whole-workspace linked program is where `@test` blocks live; a link/parse failure is a
    // compile error for the suite.
    let linked = noeta_db::linked(&p.db, p.ws);
    let program = match &linked.0 {
        Ok(program) => program,
        Err(diags) => return empty(map_diagnostics(&source_map, diags)),
    };

    let activated = noeta_check::activate_tiers(program, &["test"]);
    if !activated.diagnostics.is_empty() {
        return empty(map_diagnostics(&source_map, &activated.diagnostics));
    }
    let checked = noeta_check::check_all_with_editions(
        &activated.program,
        noeta_db::workspace_editions(&p.db, p.ws),
    );
    if !checked.diagnostics.is_empty() {
        return empty(map_diagnostics(&source_map, &checked.diagnostics));
    }

    // Setup shared by every case: the program's declarations and top-level bindings, minus its own
    // "main" effect statements (so the file's `echo`s don't run and cases can't observe each other).
    let setup: Vec<Stmt> = activated
        .program
        .stmts
        .iter()
        .filter(|s| is_tier_setup(s))
        .cloned()
        .collect();

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut cases = Vec::new();
    for test_fn in &activated.tests {
        if !matches_filter(test_fn, filter) {
            continue;
        }
        if is_skipped(test_fn) {
            skipped += 1;
            cases.push(TestCaseResult {
                name: skip_label(test_fn),
                status: "skip".to_string(),
                message: None,
                stdout: String::new(),
            });
            continue;
        }
        for case in expand_cases(test_fn) {
            let result = run_case(&setup, &case, activated.program.span, real);
            match result.status.as_str() {
                "pass" => passed += 1,
                _ => failed += 1,
            }
            cases.push(result);
        }
    }

    TestOutput {
        ok: failed == 0,
        diagnostics: Vec::new(),
        passed,
        failed,
        skipped,
        cases,
    }
}

/// One runnable `@test` invocation: the fn to call, the report label, and an optional `#[Data]` arg.
struct Case {
    fn_name: String,
    display: String,
    arg: CaseArg,
    span: noeta_span::Span,
}

/// A case's argument: none, a `#[Data]` row value, or a row literal that cannot become a value.
enum CaseArg {
    None,
    Value(Expr),
    Invalid(String),
}

/// Run one case as a fresh sandbox/real [`VmSession`] entry: `setup` + a call to the test fn (with
/// its `#[Data]` arg, if any). A diagnostic (assertion/panic) or a nonzero exit is a failure.
fn run_case(setup: &[Stmt], case: &Case, span: noeta_span::Span, real: bool) -> TestCaseResult {
    let args = match &case.arg {
        CaseArg::None => Vec::new(),
        CaseArg::Value(expr) => vec![expr.clone()],
        CaseArg::Invalid(message) => {
            return TestCaseResult {
                name: case.display.clone(),
                status: "fail".to_string(),
                message: Some(message.clone()),
                stdout: String::new(),
            };
        }
    };
    let mut stmts = setup.to_vec();
    stmts.push(call_stmt(&case.fn_name, args, case.span));
    let program = Program { stmts, span };

    let mut session = VmSession::new(session_factory(real));
    let out: SessionOutput = session.eval(&program);
    let passed = out.diagnostics.is_empty() && out.trace.is_empty();
    let message = (!passed).then(|| {
        out.diagnostics
            .first()
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "the test aborted".to_string())
    });
    TestCaseResult {
        name: case.display.clone(),
        status: if passed { "pass" } else { "fail" }.to_string(),
        message,
        stdout: out.stdout,
    }
}

/// Expand a test fn into its cases: one zero-arg case, or one per `#[Data([…])]` row.
fn expand_cases(test: &TierFn) -> Vec<Case> {
    let base = display_name(test);
    let Some(rows) = data_rows(test) else {
        return vec![Case {
            fn_name: test.name.clone(),
            display: base,
            arg: CaseArg::None,
            span: test.span,
        }];
    };
    rows.iter()
        .map(|row| Case {
            fn_name: test.name.clone(),
            display: format!("{base}[{}]", value_label(row)),
            arg: match attr_value_to_expr(row, test.span) {
                Some(expr) => CaseArg::Value(expr),
                None => CaseArg::Invalid(format!(
                    "`#[Data]` row `{}` is not a runtime value",
                    value_label(row)
                )),
            },
            span: test.span,
        })
        .collect()
}

/// A test matches the `filter` when it is absent, or when the test's display name or `#[Group]`
/// contains it (case-insensitive).
fn matches_filter(test: &TierFn, filter: Option<&str>) -> bool {
    let Some(needle) = filter.map(str::to_lowercase).filter(|n| !n.is_empty()) else {
        return true;
    };
    display_name(test).to_lowercase().contains(&needle)
        || group(test).is_some_and(|g| g.to_lowercase().contains(&needle))
}

fn is_skipped(test: &TierFn) -> bool {
    test.attrs
        .iter()
        .any(|a| a.name == noeta_ast::reflect::TEST_ATTR_SKIP)
}

fn skip_label(test: &TierFn) -> String {
    let name = display_name(test);
    match string_attr(test, noeta_ast::reflect::TEST_ATTR_SKIP) {
        Some(reason) if !reason.is_empty() => format!("{name} ({reason})"),
        _ => name,
    }
}

fn display_name(test: &TierFn) -> String {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_NAME).unwrap_or_else(|| test.name.clone())
}

fn group(test: &TierFn) -> Option<String> {
    string_attr(test, noeta_ast::reflect::TEST_ATTR_GROUP)
}

fn string_attr(test: &TierFn, name: &str) -> Option<String> {
    let attr: &Attribute = test.attrs.iter().find(|a| a.name == name)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::Str(s) => Some(s.clone()),
        _ => None,
    })
}

fn data_rows(test: &TierFn) -> Option<Vec<AttrValue>> {
    let attr = test
        .attrs
        .iter()
        .find(|a| a.name == noeta_ast::reflect::TEST_ATTR_DATA)?;
    attr.args.iter().find_map(|arg| match &arg.value {
        AttrValue::List(items) => Some(items.clone()),
        _ => None,
    })
}

/// A short label for a `#[Data]` row, for the `name[row]` case display.
fn value_label(value: &AttrValue) -> String {
    match value {
        AttrValue::Str(s) => format!("{s:?}"),
        AttrValue::Int(n) => n.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::List(items) => format!(
            "[{}]",
            items.iter().map(value_label).collect::<Vec<_>>().join(", ")
        ),
        _ => "?".to_string(),
    }
}

/// Convert a `#[Data]` row literal to an argument expression. Scalars and (recursively) lists are
/// supported; other literal forms return `None` and surface as a failing case.
fn attr_value_to_expr(value: &AttrValue, span: noeta_span::Span) -> Option<Expr> {
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

/// A statement calling fn `name` with `args`: `name(args…);`.
fn call_stmt(name: &str, args: Vec<Expr>, span: noeta_span::Span) -> Stmt {
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

/// Whether a top-level statement is tier-runner *setup* (a declaration or global binding the tests
/// depend on) as opposed to the program's own "main" effects (which `noeta test` does not run).
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

/// A host + async-executor pair — what an execution runs against.
pub(crate) type HostPair = (Box<dyn noeta_stdlib::Host>, Box<dyn noeta_stdlib::Executor>);

/// The [`HostPair`] for an execution: the deterministic sandbox, or the real host (with the
/// program's argument vector) when `real`. String errors so the debug-session run thread (which
/// has no `ErrorData` to answer with) can share it.
pub(crate) fn make_host(real: bool, args: Vec<String>) -> Result<HostPair, String> {
    if real {
        let host = noeta_runtime::RealHost::new()
            .map_err(|e| format!("cannot start the real host: {e}"))?
            .with_args(args);
        let executor = noeta_runtime::RealExecutor::new()
            .map_err(|e| format!("cannot start the async executor: {e}"))?;
        Ok((Box::new(host), Box::new(executor)))
    } else {
        Ok((
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        ))
    }
}

/// A [`VmSession`] host factory for `eval`/`test`: sandbox, or real. `eval`/`test` never pass program
/// args, so the real host is argument-free (a fragment/test sees the process's own argv, empty here).
fn session_factory(real: bool) -> noeta_vm::HostFactory {
    if real {
        Box::new(|| {
            let host: Box<dyn noeta_stdlib::Host> =
                Box::new(noeta_runtime::RealHost::new().expect("cannot start the real host"));
            let executor: Box<dyn noeta_stdlib::Executor> = Box::new(
                noeta_runtime::RealExecutor::new().expect("cannot start the async executor"),
            );
            (host, executor)
        })
    } else {
        Box::new(|| {
            (
                Box::new(noeta_stdlib::SandboxHost::new()),
                Box::new(noeta_stdlib::SandboxExecutor::new()),
            )
        })
    }
}

fn host_label(real: bool) -> String {
    if real { "real" } else { "sandbox" }.to_string()
}

/// Resolve a diagnostic slice to the canonical `JsonDiagnostic` shape (file + line/column), the same
/// form `check` returns.
fn map_diagnostics(
    source_map: &SourceMap,
    diagnostics: &[noeta_diagnostics::Diagnostic],
) -> Vec<JsonDiagnostic> {
    diagnostics.iter().map(|d| to_json(source_map, d)).collect()
}

/// Truncate `text` to at most `cap` bytes on a UTF-8 boundary, returning it and whether it was cut.
fn truncate_utf8(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    fn prep(src: &str) -> Prepared {
        noeta_stdlib::registry::default_seeded();
        prepare(&Some(src.to_string()), &None).unwrap()
    }

    #[test]
    fn run_prints_stdout_and_exits_clean() {
        // A Noeta program is its top-level statements (there is no auto-invoked `main`).
        let p = prep("echo \"hello\";\n");
        let out = run(&p, Vec::new(), false, &RunLimits::default()).unwrap();
        assert!(out.ran);
        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
        assert_eq!(out.host, "sandbox");
        assert!(out.stdout.contains("hello"), "stdout was {:?}", out.stdout);
        assert!(out.limit_hit.is_none());
    }

    #[test]
    fn run_does_not_execute_a_type_error() {
        let p = prep("fn f(): int {\n  return \"x\";\n}\n");
        let out = run(&p, Vec::new(), false, &RunLimits::default()).unwrap();
        assert!(!out.ran, "a type error must not run");
        assert!(!out.ok);
        assert!(out.diagnostics.iter().any(|d| d.code.starts_with('E')));
    }

    #[test]
    fn run_stops_an_infinite_loop_at_the_step_limit() {
        // A tight top-level loop with no exit: the instruction budget must terminate it in-VM.
        let p = prep("mut n = 0;\nwhile true {\n  n = n + 1;\n}\n");
        let limits = RunLimits {
            max_steps: Some(100_000),
            ..Default::default()
        };
        let out = run(&p, Vec::new(), false, &limits).unwrap();
        assert_eq!(out.limit_hit.as_deref(), Some("step_limit"));
        assert!(!out.ok);
        assert!(out.steps >= 100_000);
    }

    #[test]
    fn run_truncates_output_past_the_cap() {
        let p = prep("mut n = 0;\nwhile n < 100 {\n  echo \"line\";\n  n = n + 1;\n}\n");
        let limits = RunLimits {
            output_bytes: Some(20),
            ..Default::default()
        };
        let out = run(&p, Vec::new(), false, &limits).unwrap();
        assert!(out.stdout_truncated);
        assert!(out.stdout.len() <= 20);
    }

    #[test]
    fn eval_reports_value_and_type() {
        noeta_stdlib::registry::default_seeded();
        let out = eval("1 + 2", None, false);
        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
        assert_eq!(out.value.as_deref(), Some("3"));
        assert_eq!(out.r#type.as_deref(), Some("int"));
    }

    #[test]
    fn eval_uses_the_context() {
        noeta_stdlib::registry::default_seeded();
        let out = eval("xs.len()", Some("xs = [10, 20, 30];"), false);
        assert!(out.ok, "diagnostics: {:?}", out.diagnostics);
        assert_eq!(out.value.as_deref(), Some("3"));
    }

    #[test]
    fn eval_surfaces_a_parse_error() {
        noeta_stdlib::registry::default_seeded();
        let out = eval("1 +", None, false);
        assert!(!out.ok);
        assert!(!out.diagnostics.is_empty());
    }

    #[test]
    fn test_runs_at_test_cases() {
        noeta_stdlib::registry::default_seeded();
        let src = "\
fn add(a: int, b: int): int { return a + b; }

@test fn adds(): void { assert(add(2, 3) == 5); }
@test fn fails(): void { assert(add(2, 2) == 5); }
";
        let p = prep(src);
        let out = test(&p, None, false);
        assert!(out.diagnostics.is_empty(), "compile: {:?}", out.diagnostics);
        assert_eq!(out.passed, 1);
        assert_eq!(out.failed, 1);
        assert!(!out.ok);
        let adds = out.cases.iter().find(|c| c.name == "adds").unwrap();
        assert_eq!(adds.status, "pass");
    }
}
