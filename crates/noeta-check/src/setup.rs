//! **The tier runners' shared-setup filter**: which of a program's top-level statements a tier
//! runner (`noeta test`, `noeta bench`, the MCP execute path) runs before every case.
//!
//! Each case runs as `<shared setup> + <call the tier fn>` in a fresh isolate, so the setup must be
//! everything the tier fns depend on and nothing that would run the *program*. The two halves of
//! that sentence used to be decided by **statement form**: keep bindings and declarations, drop
//! `Stmt::Expr`/`If`/`For`/`While`. The drop half was there for a real reason — a CLI entry's
//! top-level `os.exit(run())` and a server's `server.serve(…)` are both statement-expressions, and
//! running either under `noeta test` would exit the runner or block it forever — but it threw out
//! everything else in the same category:
//!
//! ```noeta ignore
//! conn = db.connect("sqlite::memory:")   // Stmt::Binding  — KEPT
//! conn.migrate("migrations")             // Stmt::Expr     — silently DROPPED
//! ```
//!
//! Every test then got a live, working, **empty** database and failed with the database's own
//! `no such table: users`. No language diagnostic anywhere, because the runner had no way to ask
//! the question it actually cared about: *does this call return?*
//!
//! That question is now expressible. [`noeta_types::Type::Never`] is the type of a call that does
//! not return, the checker records each such statement in
//! [`Checked::diverging_stmts`](crate::Checked::diverging_stmts), and this module reads that set
//! instead of guessing from syntax. `conn.migrate(…)` runs; `server.serve(…)` and `os.exit(…)`
//! do not.
//!
//! ## No new expression walk
//!
//! [`ast_walk_coverage`](../../noeta_loader/ast_walk_coverage/index.html) records that ~16
//! independent `Expr` walks are the root cause of a whole silent-wrong-answer bug class here, so a
//! seventeenth would be a real cost. There is none. [`setup_drop`] recurses over **statements**
//! only — into the bodies of `if`/`for`/`while`/`concurrent`, which execute at top level, and never
//! into a declaration's body, which does not — and reads the per-statement divergence answer the
//! checker already computed while typing the expression. [`mutated_names`] is the one place that
//! looks *inside* an expression, and it looks at exactly two shapes (a call's receiver root and a
//! binding's target) to word a warning; it is not a walk and nothing depends on it being complete.

use std::collections::HashSet;

use noeta_ast::{Expr, FnDecl, Program, Stmt};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;

/// Why the shared setup does not run a top-level statement. `None` from [`setup_drop`] means the
/// statement **is** setup and runs before every case.
///
/// Every variant is a *decision with a reason*, which is the point: a runner can name what it
/// dropped and why, where the old form-based filter could only be silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupDrop {
    /// An `echo` — the program's own output. Dropped **by design**: a test run reports tests, not
    /// the program's console output, and `echo` binds nothing a test could depend on. Documented in
    /// `docs/Testing.md` and regression-tested; this is the one drop that is not a limitation.
    Output,
    /// The statement calls something that **does not return** (a `never` return type):
    /// `os.exit(code)`, `server.serve(port, fetch)`. Running it as setup would exit the runner or
    /// block it forever — this is the drop the whole filter exists for, and now the only one
    /// justified by the language rather than by syntax.
    Diverges,
    /// A `while true { … }` with no `break` that targets it — a loop that provably never exits, so
    /// the runner would sit in it forever. Only *this* loop shape is dropped; an ordinary
    /// condition-driven `while` is setup and runs. Reported — see [`dropped_setup_warnings`].
    UnboundedLoop,
    /// `return` / `break` / `continue` at the top level — control flow with no enclosing target in
    /// the synthesized per-case program. Dropped because there is nothing for it to do, not because
    /// of what it might mutate.
    ControlFlow,
}

impl SetupDrop {
    /// A short phrase naming the reason, for a warning line.
    pub fn reason(self) -> &'static str {
        match self {
            SetupDrop::Output => "it is program output, not setup",
            SetupDrop::Diverges => "it does not return (`never`), so running it would not finish",
            SetupDrop::UnboundedLoop => "`while true` with no `break` never exits",
            SetupDrop::ControlFlow => "top-level control flow has nothing to act on here",
        }
    }
}

/// Whether a top-level statement is tier-runner **setup** — a declaration, a global binding, or an
/// effect the tier fns may depend on — as opposed to something the runner must not run.
///
/// `diverging` is [`Checked::diverging_stmts`](crate::Checked::diverging_stmts) from the check of
/// the *same* program. Passing an empty set is not a neutral default: it asserts that nothing
/// diverges, and a program whose top level calls `server.serve(…)` would then hang the runner. Pass
/// the real set.
pub fn is_tier_setup(stmt: &Stmt, diverging: &HashSet<Span>) -> bool {
    setup_drop(stmt, diverging).is_none()
}

/// Why the shared setup drops `stmt`, or `None` when it runs it.
///
/// The kept forms are everything that either declares a name or performs an effect that finishes:
/// `use`/`namespace`, every declaration, bindings and destructures, `concurrent` blocks, tier
/// blocks, **and** — the change this module is for — statement-expressions, `if`, and `for`.
///
/// `if` and `for` are kept because they terminate structurally: a conditional runs one branch once,
/// and a `for` walks an iterable. A fixture seeded by a top-level `for` is ordinary setup and used
/// to vanish. They are dropped only when they *contain* something that does not finish, which is
/// the same question asked one level down. `while` is kept on the same terms, except for the one
/// shape that provably never exits (see [`SetupDrop::UnboundedLoop`]).
///
/// # Direction, and why this is not [`crate::subst::stmt_diverges`]
///
/// The two analyses ask about the same property from opposite sides, and both are sound *for their
/// own consumer*:
///
/// - `stmt_diverges` is **must-diverge**, and under-approximates: E0048 may only reject a function
///   it is *sure* falls off its end, so an unrecognized construct must count as falling through.
/// - This is **may-diverge**, and over-approximates: the runner may only run a statement it is sure
///   finishes, so anything doubtful is dropped — dropping is now *reported*
///   ([`dropped_setup_warnings`]), while a wrong "it finishes" hangs the runner with no output at
///   all.
///
/// They share the divergence *vocabulary* ([`body_breaks`](crate::subst::body_breaks), the
/// `never` type) rather than one implementation, because collapsing them would force one consumer
/// onto the other's unsound side.
///
/// # Residual holes
///
/// Divergence is **declared, not inferred**, so a call that reaches a non-returning function
/// *indirectly* is not seen: `fn boot(): void { os.exit(0) }` with a top-level `boot()` types as
/// `void`, joins the setup, and exits the runner. The fix is local and checkable — declare
/// `fn boot(): never` — and the failure is loud (the run ends) rather than the silent wrong answer
/// this module exists to remove. Likewise, a `for` over an endless generator, and a `while` whose
/// condition happens never to become false, are kept and would not finish.
pub fn setup_drop(stmt: &Stmt, diverging: &HashSet<Span>) -> Option<SetupDrop> {
    match stmt {
        Stmt::Echo { .. } => Some(SetupDrop::Output),
        Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {
            Some(SetupDrop::ControlFlow)
        }
        // The behavioural question, answered by the type system rather than by the statement's
        // shape: `conn.migrate(…)` returns and runs; `os.exit(…)` does not and does not.
        Stmt::Expr { span, .. } => diverging.contains(span).then_some(SetupDrop::Diverges),
        // `while true { … }` with no `break` targeting it is the hand-rolled event loop — the one
        // loop shape the runner must never enter. Every *other* `while` is ordinary setup and runs.
        //
        // The `true`-and-no-`break` test is not a heuristic invented here: it is the same rule
        // [`crate::subst::stmt_diverges`] uses to decide E0048 ("this function must return a
        // value"), reusing its [`body_breaks`] outright, so the compiler and the runner agree about
        // which loops end.
        Stmt::While { cond, body, .. } => {
            if matches!(cond, Expr::Bool { value: true, .. }) && !crate::subst::body_breaks(body) {
                Some(SetupDrop::UnboundedLoop)
            } else {
                body.iter()
                    .find_map(|s| setup_drop(s, diverging).filter(blocks_completion))
            }
        }
        // Bodies that DO execute at the top level, so a non-finishing statement inside them makes
        // the whole statement non-finishing. Recursion is over statements only, and deliberately
        // does not descend into declarations: `fn main() { os.exit(0) }` is a declaration the setup
        // must keep — nothing calls it.
        Stmt::If {
            then_body,
            else_body,
            ..
        } => then_body
            .iter()
            .chain(else_body.iter().flatten())
            .find_map(|s| setup_drop(s, diverging).filter(blocks_completion)),
        Stmt::For { body, .. } | Stmt::Concurrent { body, .. } => body
            .iter()
            .find_map(|s| setup_drop(s, diverging).filter(blocks_completion)),
        // Everything else declares or binds: `use`, `namespace`, `fn`/`enum`/`struct`/`class`/
        // `impl`/`trait`, bindings, destructures, `yield`, and residual tier blocks.
        Stmt::Binding { .. }
        | Stmt::Destructure { .. }
        | Stmt::Fn(_)
        | Stmt::Enum(_)
        | Stmt::Struct(_)
        | Stmt::Class(_)
        | Stmt::Impl(_)
        | Stmt::Trait(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Yield { .. }
        | Stmt::TierBlock { .. } => None,
    }
}

/// Whether a nested statement's drop reason means the **enclosing** statement would not finish
/// either. Divergence and an unbounded loop do; a nested `echo` or `break` is just a statement the
/// enclosing `if`/`for` legitimately contains and says nothing about the enclosing statement.
fn blocks_completion(drop: &SetupDrop) -> bool {
    matches!(drop, SetupDrop::Diverges | SetupDrop::UnboundedLoop)
}

/// One dropped statement a selected tier fn appears to depend on — the diagnostic that replaces the
/// silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupWarning {
    /// The dropped statement's span, for the caller to render against its own sources.
    pub span: Span,
    /// Why it was dropped.
    pub drop: SetupDrop,
    /// The top-level binding names the statement writes to, that a selected tier fn captures.
    pub names: Vec<String>,
    /// The tier fns that capture one of `names` — the tests whose result the drop can silently
    /// change.
    pub fns: Vec<String>,
}

impl SetupWarning {
    /// This warning as an ordinary [`Diagnostic`], so a runner renders it through the same path as
    /// every other diagnostic — file, line, caret, help — instead of inventing a message format of
    /// its own.
    pub fn diagnostic(&self) -> Diagnostic {
        let names = join(&self.names, '`');
        let fns = join(&self.fns, '`');
        let mut diagnostic = Diagnostic::warning(
            DiagnosticCode::DroppedTierSetup,
            self.span,
            format!(
                "this statement is not part of the shared setup ({}), but it writes to {names}, \
                 which {fns} capture{} — so {} will see {} unwritten",
                self.drop.reason(),
                if self.fns.len() == 1 { "s" } else { "" },
                if self.fns.len() == 1 {
                    "that test"
                } else {
                    "those tests"
                },
                if self.names.len() == 1 { "it" } else { "them" },
            ),
        );
        diagnostic.help(
            "do the work inside a binding (`applied = conn.migrate(\"migrations\")`) or in a helper \
             the tests call — a binding runs once per test, in that test's own isolate",
        );
        diagnostic
    }
}

/// `a`, `b` and `c`, each wrapped in `q`.
fn join(items: &[String], q: char) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("{q}{i}{q}")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// Every dropped top-level statement that writes to a binding one of the `selected` tier fns
/// **captures** — the honest reporting of the filter's residual holes.
///
/// The old filter's failure mode was not that it dropped statements; it is that a dropped statement
/// and a test that depended on it produced a plain assertion failure with no hint that a line of
/// setup had not run. This is that hint. It over-reports rather than under-reports: a name written
/// by a dropped statement *and* captured by a selected test is a real coupling even when the test
/// would have passed anyway.
///
/// [`SetupDrop::Output`] and [`SetupDrop::ControlFlow`] are excluded. `echo` binds nothing (and its
/// drop is a documented, tested rule — warning about it would be noise on every program with a
/// top-level `echo`), and top-level `return`/`break`/`continue` write nothing either.
pub fn dropped_setup_warnings(
    program: &Program,
    diverging: &HashSet<Span>,
    selected: &[&str],
) -> Vec<SetupWarning> {
    // A tier fn's captures live on its `FnDecl`, not on the `TierFn` the runner selected from, so
    // they are recovered by name from the activated program (activation lifts a `@test` block's fns
    // to the top level, so they are top-level `Stmt::Fn`s here).
    let captured: Vec<(&str, HashSet<&str>)> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Fn(decl) if selected.contains(&decl.name.as_str()) => {
                Some((decl.name.as_str(), capture_names(decl)))
            }
            _ => None,
        })
        .collect();
    if captured.is_empty() {
        return Vec::new();
    }
    program
        .stmts
        .iter()
        .filter_map(|stmt| {
            let drop = setup_drop(stmt, diverging)?;
            if matches!(drop, SetupDrop::Output | SetupDrop::ControlFlow) {
                return None;
            }
            let written = mutated_names(stmt);
            let mut names: Vec<String> = Vec::new();
            let mut fns: Vec<String> = Vec::new();
            for (name, caps) in &captured {
                let hits: Vec<&str> = written
                    .iter()
                    .filter(|w| caps.contains(w.as_str()))
                    .map(String::as_str)
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                fns.push((*name).to_string());
                for h in hits {
                    if !names.iter().any(|n| n == h) {
                        names.push(h.to_string());
                    }
                }
            }
            (!fns.is_empty()).then(|| SetupWarning {
                span: stmt.span(),
                drop,
                names,
                fns,
            })
        })
        .collect()
}

/// A fn's `use (…)` capture names.
fn capture_names(decl: &FnDecl) -> HashSet<&str> {
    decl.captures.iter().map(|(n, _)| n.as_str()).collect()
}

/// The top-level binding names a statement **writes to**, as far as two syntactic shapes can say:
/// a binding/destructure target (`x = …`, `x.f = …` and `x[i] = …` both desugar to a
/// `Stmt::Binding` on `x`), and the receiver root of a bare method call (`box.set(41)` writes
/// `box`). Recurses into executed bodies exactly as [`setup_drop`] does.
///
/// Deliberately **not** "every name mentioned". `server.serve(port, fetch)` mentions `port` and
/// `fetch`; it writes neither, and warning that a test capturing `port` depends on the dropped
/// `serve` would be false. Restricting to the two write shapes is what keeps the warning
/// trustworthy — its job is to be believed, so it must not cry wolf.
///
/// Incompleteness is acceptable here and only here: a missed name means one fewer warning about a
/// statement that was already going to be dropped, never a wrong answer about what runs.
fn mutated_names(stmt: &Stmt) -> Vec<String> {
    let mut out = Vec::new();
    collect_mutated(stmt, &mut out);
    out
}

fn collect_mutated(stmt: &Stmt, out: &mut Vec<String>) {
    let mut push = |n: String| {
        if !out.contains(&n) {
            out.push(n);
        }
    };
    match stmt {
        Stmt::Binding { name, .. } => push(name.clone()),
        Stmt::Destructure { targets, .. } => {
            for (t, _) in targets {
                push(t.clone());
            }
        }
        Stmt::Expr { expr, .. } => {
            if let Some(root) = call_receiver_root(expr) {
                push(root);
            }
        }
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            for s in then_body.iter().chain(else_body.iter().flatten()) {
                collect_mutated(s, out);
            }
        }
        Stmt::While { body, .. } | Stmt::For { body, .. } | Stmt::Concurrent { body, .. } => {
            for s in body {
                collect_mutated(s, out);
            }
        }
        // A declaration writes nothing when it is *declared*, and the remaining forms bind nothing.
        _ => {}
    }
}

/// The bare-identifier root of a method call's receiver — `box` in `box.set(41)`, `log` in
/// `log.entries.push(x)` — looking through the `await`/`?` wrappers a call can wear.
///
/// A module-qualified call (`os.exit(1)`, `server.serve(…)`) yields its module name, which this
/// cannot tell from a binding. It does not need to: the name is only ever *intersected* with a tier
/// fn's `use (…)` captures, and a capture must name a real binding (capturing a module is E0005), so
/// a module name matches nothing and drops out on its own.
fn call_receiver_root(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Await { expr, .. } | Expr::Try { expr, .. } => call_receiver_root(expr),
        Expr::Call { callee, .. } => member_root(callee),
        _ => None,
    }
}

fn member_root(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Member { receiver, .. } | Expr::Index { receiver, .. } => member_root(receiver),
        Expr::Ident { name, .. } => Some(name.to_string()),
        _ => None,
    }
}
