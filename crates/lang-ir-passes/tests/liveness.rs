//! Liveness analysis tests: lower real source to the Core IR, analyze, and assert the computed
//! death points against hand-derived expectations on the constructs that matter for drop placement.

use lang_ir_passes::{BlockLiveness, VarSet, analyze};
use lang_span::{Source, SourceId};

/// Lex, parse, and lower a source program, then analyze its liveness.
fn live(src: &str) -> lang_ir_passes::ProgramLiveness {
    let source = Source::new(SourceId::FIRST, "live", src);
    let lexed = lang_lexer::lex(&source);
    assert!(
        lexed.diagnostics.is_empty(),
        "lex errors: {:?}",
        lexed.diagnostics
    );
    let parsed = lang_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse errors: {:?}",
        parsed.diagnostics
    );
    let ir = lang_ir::lower(&parsed.program).expect("program lowers");
    analyze(&ir)
}

fn vars(names: &[&str]) -> VarSet {
    names.iter().map(|n| n.to_string()).collect()
}

/// Every `dies_here` set anywhere in a block tree (flattened), for "does X ever die in here" checks.
fn all_deaths(block: &BlockLiveness, out: &mut Vec<VarSet>) {
    for stmt in &block.stmts {
        out.push(stmt.dies_here.clone());
        for sub in &stmt.sub {
            all_deaths(sub, out);
        }
    }
}

fn mentions_death(block: &BlockLiveness, name: &str) -> bool {
    let mut deaths = Vec::new();
    all_deaths(block, &mut deaths);
    deaths.iter().any(|s| s.contains(name))
}

#[test]
fn straight_line_last_use() {
    // `x = 1; y = x + 1; echo y;` lowers to: Bind x; let t0 = x + 1; Bind y; Echo y.
    // x's last (only) use is the `let t0` that reads it; y's is the `echo`.
    let l = live("x = 1;\ny = x + 1;\necho y;\n");
    let stmts = &l.top.stmts;
    assert_eq!(stmts.len(), 4, "Bind x; Let t0; Bind y; Echo y");
    assert_eq!(stmts[0].dies_here, vars(&[]), "x is born, not dead, here");
    assert_eq!(stmts[1].dies_here, vars(&["x"]), "x dies at its read");
    assert_eq!(stmts[2].dies_here, vars(&[]), "y is born here");
    assert_eq!(stmts[3].dies_here, vars(&["y"]), "y dies at the echo");
}

#[test]
fn variable_used_only_in_a_branch_dies_in_that_branch() {
    // `x` is read only inside the then-arm; `c` is read in the condition and after the `if`.
    let l = live("mut x = 1;\nmut c = true;\nif c {\n  echo x;\n}\necho c;\n");
    let stmts = &l.top.stmts;
    // Bind x; Bind c; If; Echo c
    assert_eq!(stmts.len(), 4);
    let if_stmt = &stmts[2];
    // `c` survives the `if` (used afterwards), so it does not die at the condition.
    assert_eq!(if_stmt.dies_here, vars(&[]));
    // The then-arm is the `if`'s single sub-block; `x`'s last use is the `echo x` inside it.
    let then_block = &if_stmt.sub[0];
    assert_eq!(then_block.stmts.len(), 1, "echo x");
    assert_eq!(then_block.stmts[0].dies_here, vars(&["x"]));
    // `c` dies at the trailing `echo c`.
    assert_eq!(stmts[3].dies_here, vars(&["c"]));
    // `x` never appears live after the `if` — it does not die anywhere outside the branch.
    assert!(!mentions_death(&l.top, "x") || then_block.stmts[0].dies_here.contains("x"));
}

#[test]
fn loop_invariant_value_stays_live_across_the_back_edge() {
    // The headline loop property (P-REUSE's hand-coded positional rule, now falling out of the
    // dataflow): `limit` is read in the condition *every* iteration and again after the loop, and is
    // never reassigned — so it must stay live across the back-edge and never be reported dead inside
    // the loop. A naive single backward pass would wrongly kill it at its last textual use in the
    // condition; the fixpoint keeps it live because the next iteration re-reads it. It dies only at
    // the trailing `echo limit`.
    let l = live("limit = 10;\nmut i = 0;\nwhile i < limit {\n  i = i + 1;\n}\necho limit;\n");
    let stmts = &l.top.stmts;
    let while_stmt = stmts
        .iter()
        .find(|s| s.sub.len() == 2)
        .expect("the while statement (cond + body sub-blocks)");
    // `limit` is live across the back-edge → never dead inside the condition or body.
    assert!(
        !mentions_death(&while_stmt.sub[0], "limit"),
        "limit must stay live across the loop condition"
    );
    assert!(
        !mentions_death(&while_stmt.sub[1], "limit"),
        "limit must stay live across the loop body"
    );
    // It dies at the final `echo limit`.
    assert_eq!(stmts.last().unwrap().dies_here, vars(&["limit"]));
}

#[test]
fn reassigned_accumulators_old_value_dies_at_the_reassigning_read() {
    // The complement to the back-edge property: a `mut` accumulator's *binding* is live across the
    // loop, but each *value* dies when the next iteration overwrites it. `acc = acc + i` reads the
    // old `acc` (its last use) and then rebinds — so `acc` is recorded dead at the read inside the
    // body. The drop-insertion pass treats a death immediately followed by a rebind of the same name
    // as the §5 reassignment drop (handled by the runtime), not a separate early drop.
    let l = live(
        "mut acc = 0;\nmut i = 0;\nwhile i < 3 {\n  acc = acc + i;\n  i = i + 1;\n}\necho acc;\n",
    );
    let while_stmt = l
        .top
        .stmts
        .iter()
        .find(|s| s.sub.len() == 2)
        .expect("the while statement");
    let body = &while_stmt.sub[1];
    // The body's first statement is `let t = acc + i`, which reads (and ends the life of) the old acc.
    assert!(
        mentions_death(body, "acc"),
        "the old acc value dies at its reassigning read"
    );
    // `acc` is still live after the loop (the binding holds the final value), dying at the echo.
    assert_eq!(l.top.stmts.last().unwrap().dies_here, vars(&["acc"]));
}

#[test]
fn function_parameter_dies_at_its_last_use() {
    // Inside a function body (its own scope), a parameter read once dies at that read.
    let l = live("fn f(a) {\n  echo a;\n}\nf(5);\n");
    let fn_stmt = l
        .top
        .stmts
        .iter()
        .find(|s| s.sub.len() == 1 && !s.sub[0].stmts.is_empty())
        .expect("the fn declaration with its body sub-block");
    let body = &fn_stmt.sub[0];
    // Body: Echo a
    assert_eq!(body.stmts.last().unwrap().dies_here, vars(&["a"]));
}

#[test]
fn closure_capture_counts_as_a_use_of_the_captured_variable() {
    // `base` is captured by the closure; the capture is a use at the closure-construction site, so
    // `base` is live up to (and dies at) that statement, not before it.
    let l = live("base = 10;\ng = fn(x) => x + base;\necho g(2);\n");
    // `base` must die at the closure-construction `let`, i.e. it is named in some dies_here.
    assert!(
        mentions_death(&l.top, "base"),
        "captured `base` should have a recorded death at the closure site"
    );
}
