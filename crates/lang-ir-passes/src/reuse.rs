//! Reuse-analysis pass: an IR→IR transform that threads **in-place-reuse tokens** onto constructors
//! whose input allocation is provably dead at the construction point, so both backends can reuse the
//! storage instead of allocating afresh (memory-management migration, Phase 5).
//!
//! # What it recognizes (self-updates: record/class update + list self-append)
//!
//! The canonical, common reuse opportunity is a **self-update** — a binding rebound to a value
//! computed from its own old contents:
//!
//! ```text
//! acc = Type { ...acc, f: v }   // record/class update
//! acc = acc ~ rhs               // list self-append (the `acc ~= rhs` desugaring)
//! ```
//!
//! each of which lowers (ANF) to an adjacent pair — the op into a temp, then the reassignment:
//!
//! ```text
//! let %t = Type { ...acc, f: v }   // Rvalue::Object, spread = Var(acc)
//! acc = %t                          // Stmt::Bind, value = Temp(%t)
//!
//! let %t = acc ~ rhs                // Rvalue::Binary { op: Concat, lhs: Var(acc) }
//! acc = %t                          // Stmt::Bind, value = Temp(%t)
//! ```
//!
//! In both, the input `acc` is the **same binding** the result is bound back into, so `acc`'s old
//! value is displaced (dead) the instant the op finishes. Its allocation can therefore be reused —
//! overwriting only the changed record fields, or extending the list's backing buffer in place —
//! rather than copied. The pass marks the [`Rvalue::Object`] / [`Rvalue::Binary`] with
//! `reuse = true`.
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

use lang_ir::{Arm, Atom, BinaryOp, Block, ClassDef, Decl, Func, Program, Rvalue, Stmt, Temp};

/// Thread reuse tokens through a program, returning the annotated IR. Pure function of the input
/// (the only derived state is the set of own-destructor type names, computed from the program's
/// class declarations), so every pipeline that runs it on the same lowered+drop-annotated IR gets
/// identical tokens — the VM and the IR interpreter agree by construction.
pub fn thread_reuse(program: &Program) -> Program {
    let own_destructors = collect_own_destructors(&program.top);
    Program {
        top: rewrite_block(&program.top, &own_destructors, &HashSet::new()),
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

/// Rewrite a block: first recurse into every nested block / function body (threading the running set
/// of already-bound names so a nested control-flow block sees the names bound before it — e.g. an
/// accumulator declared before a loop), then scan the resulting statement stream for self-update /
/// reassignment-reuse pairs and set the reuse token on each. `outer_bound` is the names bound in the
/// enclosing scope; a nested block inherits them, a function body does not (it starts from its params).
fn rewrite_block(
    block: &Block,
    own_destructors: &HashSet<String>,
    outer_bound: &HashSet<String>,
) -> Block {
    let mut running = outer_bound.clone();
    let stmts: Vec<Stmt> = block
        .stmts
        .iter()
        .map(|s| {
            let out = rewrite_stmt(s, own_destructors, &running);
            if let Stmt::Bind { name, .. } = s {
                running.insert(name.clone());
            }
            out
        })
        .collect();
    Block {
        stmts: mark_self_updates(stmts, own_destructors, outer_bound),
        tail: block.tail.clone(),
    }
}

/// Mark every adjacent **self-update** pair `let %t = <op over ...acc...>` immediately followed by
/// `acc = %t`, where the input `acc` is dead the moment the op completes so its allocation may be
/// reused. Two shapes qualify:
///
/// * a record/class update `acc = Type { ...acc, … }` ([`object_self_update_candidate`]) — marked
///   unless `Type` has its own destructor (see the module note); and
/// * a list self-append `acc = acc ~ rhs` ([`concat_self_append_candidate`]) — marked when `rhs`
///   does not itself mention `acc` (else the right side would read the moved-out accumulator). Lists
///   have no own destructor and a concat destroys no element (every element lives on in the result),
///   so no destructor-exclusion is needed here.
fn mark_self_updates(
    mut stmts: Vec<Stmt>,
    own_destructors: &HashSet<String>,
    initial_bound: &HashSet<String>,
) -> Vec<Stmt> {
    // Names bound before a given point — by an earlier statement here, or in an enclosing scope
    // (`initial_bound`) — so a `Bind` of one is a **reassignment** (its old value is displaced)
    // rather than a first declaration: the condition that makes a whole-value record reassignment
    // reuse-eligible (see `object_reassign_candidate`). Seeding from the enclosing scope is what
    // catches the common loop accumulator (`mut acc` before the loop, `acc = T { … }` inside it).
    let mut bound: HashSet<String> = initial_bound.clone();
    for i in 0..stmts.len() {
        if i + 1 < stmts.len() {
            if let Some((dst, spread_name, type_name)) = object_self_update_candidate(&stmts[i]) {
                if !own_destructors.contains(&type_name)
                    && rebinds_temp(&stmts[i + 1], &spread_name, dst)
                {
                    set_object_reuse(&mut stmts[i]);
                }
            } else if let Some((dst, base_name)) = concat_self_append_candidate(&stmts[i])
                && rebinds_temp(&stmts[i + 1], &base_name, dst)
            {
                set_binary_reuse(&mut stmts[i]);
            } else if let Some((dst, base_name)) = method_self_update_candidate(&stmts[i])
                && rebinds_temp(&stmts[i + 1], &base_name, dst)
            {
                set_method_reuse(&mut stmts[i]);
            } else if let Some((dst, base_name)) = setfield_self_update_candidate(&stmts[i])
                && rebinds_temp(&stmts[i + 1], &base_name, dst)
            {
                set_setfield_reuse(&mut stmts[i]);
            } else if let Some((dst, type_name)) = object_reassign_candidate(&stmts[i])
                && !own_destructors.contains(&type_name)
                && let Some(target) = bind_target(&stmts[i + 1], dst)
                && bound.contains(&target)
            {
                // A whole-value record reassignment `x = T { … }` (all fields, no spread). Because a
                // record literal sets *every* field, it is semantically identical to the self-update
                // `x = T { ...x, … }` (the spread is fully overridden) — so injecting the `...x` spread
                // makes it a self-update the existing reuse path turns into an in-place overwrite of
                // `x`'s old cell. Sound because the type system fixes `x`'s type, so `...x` is always a
                // valid same-shape spread at this point.
                inject_object_reassign_reuse(&mut stmts[i], &target);
            }
        }
        if let Stmt::Bind { name, .. } = &stmts[i] {
            bound.insert(name.clone());
        }
    }
    stmts
}

/// The built-in collection **update methods** whose self-update `x = x.m(args)` can reuse the
/// receiver's backing buffer in place (each returns a new collection structurally derived from the
/// receiver, with value semantics). The token is name-based — a same-named *user* method is filtered
/// out at runtime by each backend's receiver-kind check, so a wrong mark only ever costs a copy.
const REUSE_METHODS: &[&str] = &["set", "remove", "add"];

/// If `stmt` is `let %t = Var(name).m(args)` for a whitelisted update method `m` whose args do not
/// mention `name`, return `(%t, name)` — the method self-update shape (`m = m.set(k, v)`, including
/// the `m[k] = v` desugaring). The receiver must be a bare source variable (the reassignable
/// accumulator); an arg mentioning it would read the slot after it is moved out, so the in-place op
/// would be unsound (left to the copying call instead).
fn method_self_update_candidate(stmt: &Stmt) -> Option<(Temp, String)> {
    let Stmt::Let {
        dst,
        rvalue:
            Rvalue::Method {
                receiver: Atom::Var { name, .. },
                name: method,
                args,
                ..
            },
        ..
    } = stmt
    else {
        return None;
    };
    if !REUSE_METHODS.contains(&method.as_str()) {
        return None;
    }
    if args.iter().any(|a| atom_is_var(a, name)) {
        return None;
    }
    Some((*dst, name.clone()))
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

/// If `stmt` is `let %t = Var(name) ~ rhs` (a list self-append by shape), return `(%t, name)`. The
/// left operand must be a bare source variable (the reassignable accumulator), and `rhs` must **not**
/// mention that variable — otherwise the right side would observe the slot after it is moved out, so
/// the in-place op would be unsound (it is left to the copying `~` instead). Only `Concat` qualifies;
/// every other operator is left untouched.
fn concat_self_append_candidate(stmt: &Stmt) -> Option<(Temp, String)> {
    let Stmt::Let {
        dst,
        rvalue:
            Rvalue::Binary {
                op: BinaryOp::Concat,
                lhs: Atom::Var { name, .. },
                rhs,
                ..
            },
        ..
    } = stmt
    else {
        return None;
    };
    if atom_is_var(rhs, name) {
        return None;
    }
    Some((*dst, name.clone()))
}

/// Whether `atom` is a bare reference to the source variable `name` — used to reject a self-append
/// whose right side mentions the accumulator (`acc ~= acc`). A temp/const operand never does (its
/// value was computed by an earlier `let`, before the accumulator is moved out).
fn atom_is_var(atom: &Atom, name: &str) -> bool {
    matches!(atom, Atom::Var { name: n, .. } if n == name)
}

/// If `stmt` is `let %t = Type { … }` with **no** spread (a whole-value record/class literal), return
/// `(%t, Type)` — a reassignment-reuse candidate (the caller still checks the result is reassigned to
/// an already-bound binding). A literal *with* a spread is a self-update, handled separately.
fn object_reassign_candidate(stmt: &Stmt) -> Option<(Temp, String)> {
    let Stmt::Let {
        dst,
        rvalue:
            Rvalue::Object {
                spread: None,
                type_name,
                ..
            },
        ..
    } = stmt
    else {
        return None;
    };
    Some((*dst, type_name.clone()))
}

/// If `stmt` is `name = %t` for the given temp, return the reassigned `name`. The dual of
/// [`rebinds_temp`] for the cases that need the bound name rather than just a yes/no.
fn bind_target(stmt: &Stmt, dst: Temp) -> Option<String> {
    match stmt {
        Stmt::Bind {
            name,
            value: Atom::Temp(t),
            ..
        } if *t == dst => Some(name.clone()),
        _ => None,
    }
}

/// Turn a whole-value record reassignment `let %t = Type { … }` into the self-update
/// `let %t = Type { ...target, … }` and mark it for reuse: the injected spread is the binding the
/// result is reassigned to, so the existing in-place path reuses its old cell (every field is
/// overwritten, since a record literal sets them all). The spread carries the object's own span.
fn inject_object_reassign_reuse(stmt: &mut Stmt, target: &str) {
    if let Stmt::Let {
        rvalue:
            Rvalue::Object {
                spread,
                reuse,
                span,
                ..
            },
        ..
    } = stmt
    {
        *spread = Some((
            Atom::Var {
                name: target.to_string(),
                span: *span,
            },
            *span,
        ));
        *reuse = true;
    }
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

/// Set the reuse token on a `let _ = lhs ~ rhs` statement (the caller has already confirmed it is a
/// [`concat_self_append_candidate`]).
fn set_binary_reuse(stmt: &mut Stmt) {
    if let Stmt::Let {
        rvalue: Rvalue::Binary { reuse, .. },
        ..
    } = stmt
    {
        *reuse = true;
    }
}

/// Set the reuse token on a `let _ = recv.m(args)` statement (the caller has already confirmed it is a
/// [`method_self_update_candidate`]).
fn set_method_reuse(stmt: &mut Stmt) {
    if let Stmt::Let {
        rvalue: Rvalue::Method { reuse, .. },
        ..
    } = stmt
    {
        *reuse = true;
    }
}

/// If `stmt` is `let %t = SetField(Var(name), …)` whose assigned **value** does not mention `name`,
/// return `(%t, name)` — the `x.f = v` field-assignment self-update shape. A value that is the bare
/// receiver (`x.f = x`) is rejected: mutating the slot in place would make the field reference the
/// post-assignment object (a self-cycle) instead of the pre-assignment value, so it is left to the
/// copying path. (A temp/const value never mentions `name` — it was computed before the move-out.)
fn setfield_self_update_candidate(stmt: &Stmt) -> Option<(Temp, String)> {
    let Stmt::Let {
        dst,
        rvalue:
            Rvalue::SetField {
                receiver: Atom::Var { name, .. },
                value,
                ..
            },
        ..
    } = stmt
    else {
        return None;
    };
    if atom_is_var(value, name) {
        return None;
    }
    Some((*dst, name.clone()))
}

fn set_setfield_reuse(stmt: &mut Stmt) {
    if let Stmt::Let {
        rvalue: Rvalue::SetField { reuse, .. },
        ..
    } = stmt
    {
        *reuse = true;
    }
}

/// Recurse into a statement's nested blocks and function bodies, returning it with reuse tokens
/// threaded throughout. Straight-line statements with no sub-structure are returned unchanged.
fn rewrite_stmt(stmt: &Stmt, od: &HashSet<String>, bound: &HashSet<String>) -> Stmt {
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
            then_block: rewrite_block(then_block, od, bound),
            else_block: else_block.as_ref().map(|b| rewrite_block(b, od, bound)),
            span: *span,
        },
        Stmt::While { cond, body, span } => Stmt::While {
            cond: rewrite_block(cond, od, bound),
            body: rewrite_block(body, od, bound),
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
            body: rewrite_block(body, od, bound),
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
                    body: rewrite_block(&arm.body, od, bound),
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
            right: rewrite_block(right, od, bound),
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
            fallback: rewrite_block(fallback, od, bound),
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

/// Rewrite a function/method/destructor body, threading reuse tokens through it. A function body
/// starts a **fresh** bound scope seeded with its parameters (it does not inherit the enclosing
/// block's names): a closure captures outer variables by cell, so reassigning a captured variable
/// inside it must not be reuse-injected (the cell may be aliased by the outer scope).
fn rewrite_func(func: &Func, od: &HashSet<String>) -> Func {
    let params: HashSet<String> = func.params.iter().cloned().collect();
    Func {
        params: func.params.clone(),
        defaults: func.defaults.clone(),
        body: rewrite_block(&func.body, od, &params),
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
