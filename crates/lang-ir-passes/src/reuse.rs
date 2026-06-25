//! Reuse-analysis pass: an IR→IR transform that threads **in-place-reuse tokens** onto constructors
//! whose input allocation is provably dead at the construction point, so both backends can reuse the
//! storage instead of allocating afresh (memory-management migration, Phase 5).
//!
//! # What it recognizes (this slice: record/class self-update)
//!
//! The canonical, common reuse opportunity is a **self-update**:
//!
//! ```text
//! acc = Type { ...acc, f: v }
//! ```
//!
//! which lowers (ANF) to an adjacent pair — the constructor into a temp, then the reassignment:
//!
//! ```text
//! let %t = Type { ...acc, f: v }   // Rvalue::Object, spread = Var(acc)
//! acc = %t                          // Stmt::Bind, value = Temp(%t)
//! ```
//!
//! Here the spread base `acc` is the **same binding** the result is bound back into, so `acc`'s old
//! value is displaced (dead) the instant the constructor finishes. Its allocation can therefore be
//! reused — overwriting only the changed fields — rather than copied. The pass marks such an
//! [`Rvalue::Object`] with `reuse = true`.
//!
//! # Why a shared IR token (not per-backend detection)
//!
//! Putting the decision on the IR both backends execute means they reuse at the **same** point by
//! construction (architecture §2): the VM lowers the marked constructor to an in-place
//! `MakeRecordInPlace`, the IR interpreter moves the base out and mutates it via `Rc::get_mut`. This
//! retires the old syntax-matched COW/record-reuse detection in favor of one IR transformation.
//!
//! # Soundness
//!
//! The token only says *where to try*; the **runtime refcount decides whether it is safe this run**
//! (`refcount == 1`). A wrong token (an aliased base) falls back to a copy — never a bug — so the
//! transform is sound even though it is a purely syntactic match.
//!
//! One semantic constraint, though, is *not* runtime-deferrable: reusing the old allocation means the
//! old value's **own `destruct` block never fires**, whereas the copy-and-destroy baseline destroys
//! the displaced base (running its destructor) on every self-update (spec §5). So a self-update is
//! only reuse-eligible when its type has **no own `destruct` block** — every record qualifies (records
//! are bodiless), and a class qualifies iff it carries no destructor. The pass therefore excludes
//! own-destructor types (derived from the IR's class declarations). The *changed* field's displaced
//! value still has its destructor fired on overwrite, by both backends — keeping reuse observationally
//! identical to copy-and-destroy, so the differential stays in agreement.

use std::collections::HashSet;
use std::rc::Rc;

use lang_ir::{Arm, Atom, Block, ClassDef, Decl, Func, Program, Rvalue, Stmt, Temp};

/// Thread reuse tokens through a program, returning the annotated IR. Pure function of the input
/// (the only derived state is the set of own-destructor type names, computed from the program's
/// class declarations), so every pipeline that runs it on the same lowered+drop-annotated IR gets
/// identical tokens — the VM and the IR interpreter agree by construction.
pub fn thread_reuse(program: &Program) -> Program {
    let own_destructors = collect_own_destructors(&program.top);
    Program {
        top: rewrite_block(&program.top, &own_destructors),
        temp_count: program.temp_count,
        span: program.span,
    }
}

/// The names of types with their **own** `destruct` block — classes whose `ClassDef` carries a
/// destructor. A self-update of one of these is *not* reuse-eligible (reuse would skip the
/// per-update destructor; see the module note). Records never have destructors, so they never
/// appear here.
fn collect_own_destructors(block: &Block) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_own_destructors_into(block, &mut out);
    out
}

/// Walk a block (and the function/method bodies it nests) collecting every own-destructor class
/// name. Classes are top-level in practice, but recursing keeps the gate correct if one is ever
/// declared in a nested scope.
fn collect_own_destructors_into(block: &Block, out: &mut HashSet<String>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Decl(Decl::Class(class)) => {
                if class.destructor.is_some() {
                    out.insert(class.decl.name.clone());
                }
                for (_, f) in &class.methods {
                    collect_own_destructors_into(&f.body, out);
                }
            }
            Stmt::Decl(Decl::Fn { func, .. }) => collect_own_destructors_into(&func.body, out),
            Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                collect_own_destructors_into(then_block, out);
                if let Some(b) = else_block {
                    collect_own_destructors_into(b, out);
                }
            }
            Stmt::While { cond, body, .. } => {
                collect_own_destructors_into(cond, out);
                collect_own_destructors_into(body, out);
            }
            Stmt::For { body, .. } => collect_own_destructors_into(body, out),
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    collect_own_destructors_into(&arm.body, out);
                }
            }
            Stmt::Logical { right, .. } => collect_own_destructors_into(right, out),
            Stmt::Coalesce { fallback, .. } => collect_own_destructors_into(fallback, out),
            _ => {}
        }
    }
}

/// Rewrite a block: first recurse into every nested block / function body, then scan the resulting
/// statement stream for adjacent self-update pairs and set the reuse token on each.
fn rewrite_block(block: &Block, own_destructors: &HashSet<String>) -> Block {
    let stmts: Vec<Stmt> = block
        .stmts
        .iter()
        .map(|s| rewrite_stmt(s, own_destructors))
        .collect();
    Block {
        stmts: mark_self_updates(stmts, own_destructors),
        tail: block.tail.clone(),
    }
}

/// Set `reuse = true` on every `let %t = Type { ...acc, … }` immediately followed by `acc = %t` —
/// the self-update shape, where the spread base is dead the moment the constructor completes — as
/// long as `Type` has no own destructor (see the module note).
fn mark_self_updates(mut stmts: Vec<Stmt>, own_destructors: &HashSet<String>) -> Vec<Stmt> {
    for i in 0..stmts.len().saturating_sub(1) {
        let Some((dst, spread_name, type_name)) = object_self_update_candidate(&stmts[i]) else {
            continue;
        };
        if own_destructors.contains(&type_name) {
            continue;
        }
        if rebinds_temp(&stmts[i + 1], &spread_name, dst) {
            set_object_reuse(&mut stmts[i]);
        }
    }
    stmts
}

/// If `stmt` is `let %t = Type { ...Var(name), … }` (a reuse candidate by shape), return
/// `(%t, name, Type)`. The spread must be a bare source variable — a temp or constant spread is not
/// a reassignable binding, so it cannot be a self-update.
fn object_self_update_candidate(stmt: &Stmt) -> Option<(Temp, String, String)> {
    let Stmt::Let {
        dst,
        rvalue: Rvalue::Object {
            spread, type_name, ..
        },
        ..
    } = stmt
    else {
        return None;
    };
    let Some((Atom::Var { name, .. }, _)) = spread else {
        return None;
    };
    Some((*dst, name.clone(), type_name.clone()))
}

/// Whether `stmt` is `name = %t` — the reassignment of the spread base to the constructor's result,
/// confirming the self-update.
fn rebinds_temp(stmt: &Stmt, name: &str, dst: Temp) -> bool {
    matches!(
        stmt,
        Stmt::Bind { name: bname, value: Atom::Temp(t), .. } if bname == name && *t == dst
    )
}

/// Set the reuse token on a `let _ = Object { … }` statement (the caller has already confirmed it is
/// an [`object_self_update_candidate`]).
fn set_object_reuse(stmt: &mut Stmt) {
    if let Stmt::Let {
        rvalue: Rvalue::Object { reuse, .. },
        ..
    } = stmt
    {
        *reuse = true;
    }
}

/// Recurse into a statement's nested blocks and function bodies, returning it with reuse tokens
/// threaded throughout. Straight-line statements with no sub-structure are returned unchanged.
fn rewrite_stmt(stmt: &Stmt, od: &HashSet<String>) -> Stmt {
    match stmt {
        Stmt::Let { dst, rvalue, span } => Stmt::Let {
            dst: *dst,
            rvalue: rewrite_rvalue(rvalue, od),
            span: *span,
        },
        Stmt::Eval { rvalue, span } => Stmt::Eval {
            rvalue: rewrite_rvalue(rvalue, od),
            span: *span,
        },
        Stmt::If {
            cond,
            then_block,
            else_block,
            span,
        } => Stmt::If {
            cond: cond.clone(),
            then_block: rewrite_block(then_block, od),
            else_block: else_block.as_ref().map(|b| rewrite_block(b, od)),
            span: *span,
        },
        Stmt::While { cond, body, span } => Stmt::While {
            cond: rewrite_block(cond, od),
            body: rewrite_block(body, od),
            span: *span,
        },
        Stmt::For {
            pattern,
            iterable,
            body,
            span,
        } => Stmt::For {
            pattern: pattern.clone(),
            iterable: iterable.clone(),
            body: rewrite_block(body, od),
            span: *span,
        },
        Stmt::Match {
            scrutinee,
            arms,
            dst,
            span,
        } => Stmt::Match {
            scrutinee: scrutinee.clone(),
            arms: arms
                .iter()
                .map(|arm| Arm {
                    pattern: arm.pattern.clone(),
                    body: rewrite_block(&arm.body, od),
                    span: arm.span,
                })
                .collect(),
            dst: *dst,
            span: *span,
        },
        Stmt::Logical {
            dst,
            op,
            left,
            right,
            span,
        } => Stmt::Logical {
            dst: *dst,
            op: *op,
            left: left.clone(),
            right: rewrite_block(right, od),
            span: *span,
        },
        Stmt::Coalesce {
            dst,
            value,
            fallback,
            span,
        } => Stmt::Coalesce {
            dst: *dst,
            value: value.clone(),
            fallback: rewrite_block(fallback, od),
            span: *span,
        },
        Stmt::Decl(decl) => Stmt::Decl(rewrite_decl(decl, od)),
        Stmt::Bind { .. }
        | Stmt::Echo { .. }
        | Stmt::Return { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. } => stmt.clone(),
    }
}

/// Recurse into a [`Rvalue::Closure`]'s body; every other rvalue carries no nested block.
fn rewrite_rvalue(rvalue: &Rvalue, od: &HashSet<String>) -> Rvalue {
    match rvalue {
        Rvalue::Closure { func, span } => Rvalue::Closure {
            func: Rc::new(rewrite_func(func, od)),
            span: *span,
        },
        other => other.clone(),
    }
}

/// Rewrite a function/method/destructor body, threading reuse tokens through it.
fn rewrite_func(func: &Func, od: &HashSet<String>) -> Func {
    Func {
        params: func.params.clone(),
        defaults: func.defaults.clone(),
        body: rewrite_block(&func.body, od),
        temp_count: func.temp_count,
        span: func.span,
    }
}

fn rewrite_decl(decl: &Decl, od: &HashSet<String>) -> Decl {
    match decl {
        Decl::Fn { name, func, span } => Decl::Fn {
            name: name.clone(),
            func: Rc::new(rewrite_func(func, od)),
            span: *span,
        },
        Decl::Class(class) => Decl::Class(ClassDef {
            decl: class.decl.clone(),
            methods: class
                .methods
                .iter()
                .map(|(n, f)| (n.clone(), Rc::new(rewrite_func(f, od))))
                .collect(),
            destructor: class
                .destructor
                .as_ref()
                .map(|f| Rc::new(rewrite_func(f, od))),
            span: class.span,
        }),
        Decl::Enum(_) | Decl::Record(_) | Decl::Use { .. } => decl.clone(),
    }
}
