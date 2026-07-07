//! **Session parity**: the VM REPL (`noeta_vm::VmSession`) and the tree-walker REPL
//! (`noeta_eval::Session`) must tell the same story across a *sequence* of entries — not just for a
//! single program, but with persistent state carried between entries (bindings, functions, the id
//! counter, live objects, a mid-session panic, `:drop`).
//!
//! The program-level differential (`run_differential`) only ever ran a *whole* program through each
//! backend once; it never exercised state that survives across batches. This is the oracle for that:
//! it feeds an identical script of REPL steps to both sessions and asserts each step's observable
//! output agrees. It is the safety net that lets the CLI's REPL move onto the VM (R3) and the
//! tree-walker be cut from the shipped binary while staying provably equivalent — the oracle lives on
//! here, in a test-only crate, exactly where it belongs.
//!
//! `:type` is deliberately **not** compared: on the VM it reports the reflected surface type
//! (`List<int>`), where the tree-walker erases to the head constructor (`list`) — an intentional
//! improvement, not a divergence, so it is out of this oracle's scope.

use noeta_backend::TraceFrame;
use noeta_diagnostics::Diagnostic;
use noeta_eval::Session as EvalSession;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmSession;

/// One REPL step, run against both sessions.
enum Step {
    /// Evaluate an entry (compare stdout, echoed value, diagnostics, and the panic trace's names).
    Eval(&'static str),
    /// `:drop <name>` (compare the destructor output and whether a binding existed).
    Drop(&'static str),
}

fn program(src: &str) -> noeta_ast::Program {
    let source = Source::new(SourceId::FIRST, "<repl>", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "test source should parse cleanly: {src:?}"
    );
    parsed.program
}

/// The abort traceback projected to its function-name story (innermost first). Compared by name, not
/// span/line: across a multi-entry session both backends reuse `SourceId::FIRST`, so a frame from an
/// *earlier* entry carries a stale span — the per-program line parity is covered by `trace_parity`.
fn trace_names(trace: &[TraceFrame]) -> Vec<Option<String>> {
    trace.iter().map(|f| f.name.clone()).collect()
}

fn diagnostics(diags: &[Diagnostic]) -> &[Diagnostic] {
    diags
}

/// Drive an identical script through both sessions and assert every step agrees.
fn assert_sessions_agree(script: &[Step]) {
    let mut eval = EvalSession::new();
    let mut vm = VmSession::new(Box::new(|| {
        (
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        )
    }));

    for (i, step) in script.iter().enumerate() {
        let (e, v) = match step {
            Step::Eval(src) => {
                let prog = program(src);
                (eval.eval(&prog), vm.eval(&prog))
            }
            Step::Drop(name) => {
                let (e_found, e_out) = eval.drop_binding(name);
                let (v_found, v_out) = vm.drop_binding(name);
                assert_eq!(
                    e_found, v_found,
                    "step {i} `:drop {name}`: backends disagree on whether the binding existed"
                );
                (e_out, v_out)
            }
        };

        assert_eq!(e.stdout, v.stdout, "step {i}: stdout differs");
        assert_eq!(e.value, v.value, "step {i}: echoed value differs");
        assert_eq!(
            diagnostics(&e.diagnostics),
            diagnostics(&v.diagnostics),
            "step {i}: diagnostics differ"
        );
        assert_eq!(
            trace_names(&e.trace),
            trace_names(&v.trace),
            "step {i}: abort-trace name story differs"
        );
    }

    // Every live **value** binding the VM reports must also be a binding the tree-walker reports —
    // proof no value binding diverged. (The tree-walker additionally lists declared *type* names,
    // because it stores types as scope values; the VM's `:bindings` cleanly reports value globals
    // only. That difference is a UX choice, not execution semantics, so it is out of this oracle's
    // scope — the per-step stdout/value/diagnostics/trace checks above are the execution parity.)
    let eval_binds = eval.binding_names();
    for name in vm.binding_names() {
        assert!(
            eval_binds.contains(&name),
            "VM reports a value binding `{name}` the tree-walker does not: {eval_binds:?}"
        );
    }

    vm.teardown();
}

#[test]
fn persistent_bindings_functions_and_globals_agree() {
    assert_sessions_agree(&[
        Step::Eval("mut acc = 0;"),
        // A function defined in one entry mutates a global from an earlier entry...
        Step::Eval("fn bump(): int {\n  acc = acc + 1;\n  return acc;\n}"),
        // ...and is callable in later entries, seeing the persistent global.
        Step::Eval("echo bump();"),
        Step::Eval("echo bump();"),
        // A bare trailing expression echoes its value.
        Step::Eval("1 + 2"),
        // Rebinding a name updates it.
        Step::Eval("acc = 40;"),
        Step::Eval("echo bump();"),
    ]);
}

#[test]
fn destructors_and_drop_agree_across_entries() {
    assert_sessions_agree(&[
        Step::Eval(
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}",
        ),
        // Construct in one entry, store in a global.
        Step::Eval("mut r = Res.new(9);"),
        // A trailing bare expression that constructs a fresh Res is displayed, then dropped now —
        // its destructor fires at end of the step in both backends.
        Step::Eval("Res.new(1)"),
        // `:drop` runs the stored object's destructor and unbinds it.
        Step::Drop("r"),
        // Dropping a name that is not bound agrees (no output, not found).
        Step::Drop("ghost"),
    ]);
}

#[test]
fn a_mid_session_panic_traces_identically_and_the_session_survives() {
    assert_sessions_agree(&[
        Step::Eval("mut kept = 7;"),
        Step::Eval("fn boom(): int {\n  return panic(\"kaboom\");\n}"),
        // The panic aborts this entry: no stdout, a diagnostic, and a trace both backends agree on.
        Step::Eval("echo boom();"),
        // The session survives the panic — earlier state is intact in both backends.
        Step::Eval("echo kept;"),
    ]);
}

#[test]
fn cross_entry_reflection_agrees() {
    // Reflection accumulates across entries on **both** backends: an attribute declared in one entry
    // and attached in another is found by a query in a third. Without accumulation the query entry's
    // reflection would hold only its own declarations, so `attributes_of::<Column>()` would be empty —
    // this both proves the accumulation and keeps the two REPLs in lockstep.
    assert_sessions_agree(&[
        Step::Eval("@attribute\nstruct Column { name: string }"),
        Step::Eval("struct User {\n  #[Column(\"uid\")]\n  id: int\n}"),
        Step::Eval(
            "for c in attributes_of::<Column>() {\n  echo c.target;\n  echo c.value.name;\n}",
        ),
        // A type redefined in a later entry supersedes its old reflection (latest-wins): re-declaring
        // User with a different attribute replaces, not duplicates.
        Step::Eval("struct User {\n  #[Column(\"renamed\")]\n  id: int\n}"),
        Step::Eval(
            "for c in attributes_of::<Column>() {\n  echo c.target;\n  echo c.value.name;\n}",
        ),
    ]);
}

#[test]
fn cross_entry_object_identity_and_method_dispatch_agree() {
    assert_sessions_agree(&[
        Step::Eval(
            "class Box {\n  mut v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn bump(): int { self.v = self.v + 1; return self.v; }\n}",
        ),
        Step::Eval("mut b = Box.new(10);"),
        // Mutating method calls on the same object across entries accumulate identically.
        Step::Eval("echo b.bump();"),
        Step::Eval("echo b.bump();"),
        Step::Eval("mut xs = [b, Box.new(0)];"),
        Step::Eval("echo xs.len();"),
    ]);
}
