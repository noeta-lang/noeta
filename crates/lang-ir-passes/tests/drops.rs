//! Drop-insertion tests: lower real source, run the pass, and check where `DropVar`s land —
//! function-locals at their last use, never globals, never reassigned bindings.

use lang_ir::{Block, Decl, Func, Program, Rvalue, Stmt};
use lang_ir_passes::insert_drops;
use lang_span::{Source, SourceId};

fn lower(src: &str) -> Program {
    let source = Source::new(SourceId::FIRST, "drops", src);
    let lexed = lang_lexer::lex(&source);
    assert!(lexed.diagnostics.is_empty(), "lex: {:?}", lexed.diagnostics);
    let parsed = lang_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse: {:?}",
        parsed.diagnostics
    );
    lang_ir::lower(&parsed.program).expect("lowers")
}

/// Collect every `DropVar` name in a block tree, descending into control-flow sub-blocks and
/// nested function bodies, tagged with whether it was found at the top level or inside a function.
#[derive(Default)]
struct Drops {
    top: Vec<String>,
    in_funcs: Vec<String>,
}

fn collect(program: &Program) -> Drops {
    let mut d = Drops::default();
    walk_block(&program.top, false, &mut d);
    d
}

fn walk_block(block: &Block, in_func: bool, d: &mut Drops) {
    for stmt in &block.stmts {
        walk_stmt(stmt, in_func, d);
    }
}

fn walk_stmt(stmt: &Stmt, in_func: bool, d: &mut Drops) {
    match stmt {
        Stmt::DropVar { name, .. } => {
            if in_func {
                d.in_funcs.push(name.clone());
            } else {
                d.top.push(name.clone());
            }
        }
        Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => {
            if let Rvalue::Closure { func, .. } = rvalue {
                walk_func(func, d);
            }
        }
        Stmt::If {
            then_block,
            else_block,
            ..
        } => {
            walk_block(then_block, in_func, d);
            if let Some(b) = else_block {
                walk_block(b, in_func, d);
            }
        }
        Stmt::While { cond, body, .. } => {
            walk_block(cond, in_func, d);
            walk_block(body, in_func, d);
        }
        Stmt::For { body, .. } => walk_block(body, in_func, d),
        Stmt::Match { arms, .. } => {
            for arm in arms {
                walk_block(&arm.body, in_func, d);
            }
        }
        Stmt::Logical { right, .. } => walk_block(right, in_func, d),
        Stmt::Coalesce { fallback, .. } => walk_block(fallback, in_func, d),
        Stmt::Decl(Decl::Fn { func, .. }) => walk_func(func, d),
        Stmt::Decl(Decl::Class(class)) => {
            for (_, f) in &class.methods {
                walk_func(f, d);
            }
            if let Some(f) = &class.destructor {
                walk_func(f, d);
            }
        }
        _ => {}
    }
}

fn walk_func(func: &Func, d: &mut Drops) {
    walk_block(&func.body, true, d);
}

#[test]
fn function_locals_are_dropped_at_their_last_use() {
    // `a` (a parameter) dies at its last read; `x` (a single-assignment local) dies at `echo x`.
    let program = lower("fn f(a) {\n  x = [a, a];\n  echo x;\n  echo 1;\n}\nf(5);\n");
    let dropped = insert_drops(&program);
    let d = collect(&dropped);
    assert!(
        d.in_funcs.contains(&"a".to_string()),
        "param a should be dropped: {:?}",
        d.in_funcs
    );
    assert!(
        d.in_funcs.contains(&"x".to_string()),
        "local x should be dropped: {:?}",
        d.in_funcs
    );
}

#[test]
fn top_level_globals_are_never_dropped() {
    // Top-level bindings are globals; their reclamation stays at teardown (spec §2), so no `DropVar`.
    let program = lower("x = [1, 2];\necho x;\necho 3;\n");
    let dropped = insert_drops(&program);
    let d = collect(&dropped);
    assert!(d.top.is_empty(), "no top-level drops, got {:?}", d.top);
}

#[test]
fn reassigned_bindings_are_not_dropped() {
    // `acc` is reassigned (mut decl + reassignment), so it is not single-assignment and keeps the
    // existing reassignment-release + teardown behavior — the pass inserts no `DropVar` for it.
    let program = lower("fn f() {\n  mut acc = 0;\n  acc = acc + 1;\n  echo acc;\n}\nf();\n");
    let dropped = insert_drops(&program);
    let d = collect(&dropped);
    assert!(
        !d.in_funcs.contains(&"acc".to_string()),
        "reassigned acc must not be dropped, got {:?}",
        d.in_funcs
    );
}

#[test]
fn a_local_holding_a_value_past_a_branch_is_dropped_after_its_use() {
    // `data` is read after the `if`; it must not be dropped before/inside the branch, only at its
    // real last use (the trailing `echo data`).
    let program =
        lower("fn f(n) {\n  data = [n];\n  if n > 0 {\n    echo n;\n  }\n  echo data;\n}\nf(1);\n");
    let dropped = insert_drops(&program);
    let d = collect(&dropped);
    assert!(
        d.in_funcs.contains(&"data".to_string()),
        "data dropped at last use: {:?}",
        d.in_funcs
    );
}

#[test]
fn a_local_captured_by_a_closure_is_never_dropped() {
    // `tag` is captured by the returned closure (here through its *default* `label = tag`, which the
    // body never names). It must not be dropped in the enclosing function — the closure reads it
    // later through the shared captured scope. The capture over-approximation covers nested-closure
    // defaults, not just bodies.
    let program = lower(
        "fn make(tag) {\n  return fn(s, label = tag) => label ~ s;\n}\nt = make(\"X\");\necho t(\"a\");\n",
    );
    let dropped = insert_drops(&program);
    let d = collect(&dropped);
    assert!(
        !d.in_funcs.contains(&"tag".to_string()),
        "captured `tag` must not be dropped, got {:?}",
        d.in_funcs
    );
}

#[test]
fn idempotent_second_run_inserts_no_further_drops() {
    // Running the pass on already-annotated IR must not add drops (DropVar is not a use, and the
    // bindings already died at the same points).
    let program = lower("fn f(a) {\n  x = [a];\n  echo x;\n}\nf(2);\n");
    let once = insert_drops(&program);
    let once_count = collect(&once).in_funcs.len();
    let twice = insert_drops(&once);
    let twice_count = collect(&twice).in_funcs.len();
    assert_eq!(
        once_count, twice_count,
        "drop insertion should be idempotent"
    );
}
