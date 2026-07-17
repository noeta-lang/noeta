//! Drop-insertion tests: lower real source, run the pass, and check where `DropVar`s land —
//! function-locals at their last use, never globals, never reassigned bindings.

use noeta_ir::{Block, Decl, Func, Program, Rvalue, Stmt};
use noeta_ir_passes::insert_drops;
use noeta_span::{Source, SourceId};

fn lower(src: &str) -> Program {
    // Own assembling driver (audit-6 F2): seed the std units before the front-end runs.
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId::FIRST, "drops", src);
    let lexed = noeta_lexer::lex(&source);
    assert!(lexed.diagnostics.is_empty(), "lex: {:?}", lexed.diagnostics);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse: {:?}",
        parsed.diagnostics
    );
    noeta_ir::lower(&parsed.program).expect("lowers")
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

/// The body block of a top-level `fn` declaration by name.
fn func_body<'a>(program: &'a Program, name: &str) -> &'a Block {
    program
        .top
        .stmts
        .iter()
        .find_map(|s| match s {
            Stmt::Decl(Decl::Fn { name: n, func, .. }) if n == name => Some(&func.body),
            _ => None,
        })
        .expect("function declaration")
}

fn walk_func(func: &Func, d: &mut Drops) {
    walk_block(&func.body, true, d);
}

#[test]
fn function_locals_are_dropped_at_their_last_use() {
    // `a` (a parameter) dies at its last read; `x` (a single-assignment local) dies at `echo x`.
    let program = lower("fn f(a) {\n  x = [a, a];\n  echo x;\n  echo 1;\n}\nf(5);\n");
    let dropped = insert_drops(&program, None);
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
    let dropped = insert_drops(&program, None);
    let d = collect(&dropped);
    assert!(d.top.is_empty(), "no top-level drops, got {:?}", d.top);
}

#[test]
fn reassigned_bindings_survivor_is_dropped_at_scope_exit() {
    // `acc` is reassigned (mut decl + reassignment), so it is not single-assignment — the last-use
    // pass skips it. Its *intermediate* value is released by the runtime at the assignment (spec §5);
    // its *surviving* value is reclaimed by the Phase 4.2a scope-exit drop when `f` falls off its end.
    let program = lower("fn f() {\n  mut acc = 0;\n  acc = acc + 1;\n  echo acc;\n}\nf();\n");
    let dropped = insert_drops(&program, None);
    let d = collect(&dropped);
    assert!(
        d.in_funcs.contains(&"acc".to_string()),
        "reassigned acc's survivor must be dropped at scope exit, got {:?}",
        d.in_funcs
    );
}

#[test]
fn a_local_live_past_an_early_return_is_dropped_before_it() {
    // Phase 4.2b: `r`'s normal last use is past the `return`, so on the taken path it is abandoned —
    // the early-exit drop must reclaim it. The `DropVar r` lands *inside the then-block, before the
    // `Return`* (so it is reachable only on the path that actually returns).
    let program =
        lower("fn f(c) {\n  r = [1, 2];\n  if c {\n    return;\n  }\n  echo r;\n}\nf(true);\n");
    let dropped = insert_drops(&program, None);
    // Find the then-block of the `if` and assert a `DropVar r` precedes its `Return`.
    let func = func_body(&dropped, "f");
    let then_block = func
        .stmts
        .iter()
        .find_map(|s| match s {
            Stmt::If { then_block, .. } => Some(then_block),
            _ => None,
        })
        .expect("if statement");
    let drop_pos = then_block
        .stmts
        .iter()
        .position(|s| matches!(s, Stmt::DropVar { name, .. } if name == "r"));
    let ret_pos = then_block
        .stmts
        .iter()
        .position(|s| matches!(s, Stmt::Return { .. }))
        .expect("return in then-block");
    assert!(
        matches!(drop_pos, Some(d) if d < ret_pos),
        "DropVar r must precede the return in the then-block, drops: {:?}",
        then_block.stmts
    );
}

#[test]
fn an_accumulator_live_across_a_loop_is_not_dropped() {
    // The Phase 4.2a scope-exit drop must respect `live_out`: `acc` flows around the loop's back-edge
    // (read in a later iteration) and out to the `return`, so it is live at the loop body's exit and
    // must NOT be dropped there — dropping it would null a slot still in use. The trailing `return`
    // moves it out, so there is no function-exit drop either; `acc` appears in no drop set.
    let program = lower(
        "fn f() {\n  mut acc = 0;\n  while acc < 3 {\n    acc = acc + 1;\n  }\n  return acc;\n}\nf();\n",
    );
    let dropped = insert_drops(&program, None);
    let d = collect(&dropped);
    assert!(
        !d.in_funcs.contains(&"acc".to_string()),
        "accumulator live across the loop must not be dropped, got {:?}",
        d.in_funcs
    );
}

#[test]
fn a_local_holding_a_value_past_a_branch_is_dropped_after_its_use() {
    // `data` is read after the `if`; it must not be dropped before/inside the branch, only at its
    // real last use (the trailing `echo data`).
    let program =
        lower("fn f(n) {\n  data = [n];\n  if n > 0 {\n    echo n;\n  }\n  echo data;\n}\nf(1);\n");
    let dropped = insert_drops(&program, None);
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
    let dropped = insert_drops(&program, None);
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
    let once = insert_drops(&program, None);
    let once_count = collect(&once).in_funcs.len();
    let twice = insert_drops(&once, None);
    let twice_count = collect(&twice).in_funcs.len();
    assert_eq!(
        once_count, twice_count,
        "drop insertion should be idempotent"
    );
}

/// The `relevant` bit of each in-function `DropVar`, by name.
fn drop_relevance(program: &Program) -> std::collections::HashMap<String, bool> {
    let mut out = std::collections::HashMap::new();
    fn walk(block: &Block, in_func: bool, out: &mut std::collections::HashMap<String, bool>) {
        for stmt in &block.stmts {
            match stmt {
                Stmt::DropVar { name, relevant, .. } if in_func => {
                    out.insert(name.clone(), *relevant);
                }
                Stmt::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    walk(then_block, in_func, out);
                    if let Some(b) = else_block {
                        walk(b, in_func, out);
                    }
                }
                Stmt::While { body, .. } | Stmt::For { body, .. } => walk(body, in_func, out),
                Stmt::Decl(Decl::Fn { func, .. }) => walk(&func.body, true, out),
                _ => {}
            }
        }
    }
    walk(&program.top, false, &mut out);
    out
}

/// Find the `name_span` of the first `Bind` of `name` inside the first nested function body.
fn bind_span(program: &Program, name: &str) -> noeta_span::Span {
    fn find(block: &Block, name: &str) -> Option<noeta_span::Span> {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Bind {
                    name: n, name_span, ..
                } if n == name => return Some(*name_span),
                Stmt::Decl(Decl::Fn { func, .. }) => {
                    if let Some(s) = find(&func.body, name) {
                        return Some(s);
                    }
                }
                _ => {}
            }
        }
        None
    }
    find(&program.top, name).expect("a binding of that name")
}

#[test]
fn the_relevant_bit_reflects_the_relevance_oracle() {
    // `x` is a single-assignment local dropped at its last use; its `relevant` bit must follow the
    // oracle: true when its binding span is in `locals`, false when the oracle is present but omits
    // it, and true (conservative) with no oracle at all.
    let program = lower("fn f() {\n  x = [1];\n  echo x;\n}\nf();\n");
    let span = bind_span(&program, "x");

    let with = noeta_ir_passes::Relevance {
        locals: std::iter::once(span).collect(),
        params: Default::default(),
    };
    assert!(drop_relevance(&insert_drops(&program, Some(&with)))["x"]);

    let without = noeta_ir_passes::Relevance::default();
    assert!(!drop_relevance(&insert_drops(&program, Some(&without)))["x"]);

    assert!(drop_relevance(&insert_drops(&program, None))["x"]);
}
