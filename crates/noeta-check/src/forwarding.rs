//! **Type-param forwarding pre-pass** (poly-values F2b): which top-level generic functions
//! forward a type parameter into a **call-site-typed position** — a native turbofish
//! (`json.try_parse::<T>`), a reflection manifest query (`attributes_of::<T>`), or (transitively)
//! another forwarding generic (`load::<T>(p)`).
//!
//! Generics are erased at runtime, so one compiled body serves every instantiation; a forwarded
//! site therefore needs its per-instantiation data (`TypeRecipe` / type name) delivered
//! **dynamically** — as a hidden call argument indexing the program's `TypeArgInfo` table. This
//! pass computes, purely syntactically and BEFORE body checking, each function's ordered list of
//! forwarding parameters, so both the body-side sites (which read the hidden slot) and the call
//! sites (which supply it) agree on the layout.
//!
//! Scope: **top-level `fn` declarations only.** Methods carry their class's parameters (a
//! different instantiation channel) and nested `fn`s are not in the symbol table; a forwarded
//! site in either is a checker error, not silently wrong. Transitive forwarding is recognized
//! through an EXPLICIT turbofish only (`g::<T>(x)`) — a fixpoint over the call graph; forwarding
//! via argument inference alone is rejected at the call site with a "spell the turbofish" help.

use noeta_ast::{ClosureBody, Expr, FnDecl, Program, Stmt, StrPart, TypeRef};
use std::collections::HashMap;

/// One forwarding type parameter of a generic function, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardParam {
    /// The type parameter's name (`"T"`).
    pub(crate) name: String,
    /// Whether some forwarded site consumes a **build recipe** (a `TypedModuleCall` turbofish) —
    /// if so, an instantiating call must supply a recipe-capable type (checked statically at the
    /// call site). A name-only consumer (`attributes_of`) leaves this `false`.
    pub(crate) needs_recipe: bool,
}

/// The per-function forwarding table: fn name → its forwarding type parameters, in the
/// declaration's type-parameter order.
pub(crate) type ForwardingMap = HashMap<String, Vec<ForwardParam>>;

/// Compute the program's forwarding table — a fixpoint over the top-level `fn`s (a function
/// forwards transitively through a turbofish call of another forwarding function).
pub(crate) fn compute_forwarding(program: &Program) -> ForwardingMap {
    let fns: Vec<&FnDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Fn(f) if !f.type_params.is_empty() => Some(f),
            _ => None,
        })
        .collect();
    // The declaration-order type parameters of every candidate, for aligning turbofish arguments.
    let decl_params: HashMap<&str, Vec<&str>> = fns
        .iter()
        .map(|f| {
            (
                f.name.as_str(),
                f.type_params.iter().map(|p| p.name.as_str()).collect(),
            )
        })
        .collect();
    let mut map: ForwardingMap = HashMap::new();
    loop {
        let mut changed = false;
        for f in &fns {
            // Collect this pass's marks: param name → needs_recipe.
            let mut marks: HashMap<String, bool> = HashMap::new();
            {
                let mut mark_fn = |param: &str, needs_recipe: bool| {
                    let slot = marks.entry(param.to_string()).or_insert(false);
                    *slot |= needs_recipe;
                };
                let mark: &mut dyn FnMut(&str, bool) = &mut mark_fn;
                let params: Vec<&str> = f.type_params.iter().map(|p| p.name.as_str()).collect();
                for stmt in &f.body {
                    walk_stmt(stmt, &params, &map, &decl_params, mark);
                }
            }
            // Project onto declaration order and compare with the previous fixpoint state.
            let next: Vec<ForwardParam> = f
                .type_params
                .iter()
                .filter_map(|p| {
                    marks.get(&p.name).map(|&needs_recipe| ForwardParam {
                        name: p.name.clone(),
                        needs_recipe,
                    })
                })
                .collect();
            if next.is_empty() {
                continue;
            }
            if map.get(&f.name) != Some(&next) {
                map.insert(f.name.clone(), next);
                changed = true;
            }
        }
        if !changed {
            return map;
        }
    }
}

/// Whether a surface type reference is exactly the bare type parameter `param`.
fn is_bare_param(ty: &TypeRef, param: &str) -> bool {
    matches!(ty, TypeRef::Named { name, args, .. } if name == param && args.is_empty())
}

fn walk_stmt(
    stmt: &Stmt,
    params: &[&str],
    map: &ForwardingMap,
    decl_params: &HashMap<&str, Vec<&str>>,
    mark: &mut dyn FnMut(&str, bool),
) {
    match stmt {
        Stmt::Echo { value: e, .. } | Stmt::Yield { value: e, .. } => {
            walk_expr(e, params, map, decl_params, mark)
        }
        Stmt::Binding { value, .. } => walk_expr(value, params, map, decl_params, mark),
        Stmt::Destructure { value, .. } => walk_expr(value, params, map, decl_params, mark),
        Stmt::Expr { expr, .. } => walk_expr(expr, params, map, decl_params, mark),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                walk_expr(v, params, map, decl_params, mark);
            }
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            walk_expr(cond, params, map, decl_params, mark);
            for s in then_body {
                walk_stmt(s, params, map, decl_params, mark);
            }
            if let Some(b) = else_body {
                for s in b {
                    walk_stmt(s, params, map, decl_params, mark);
                }
            }
        }
        Stmt::For { iterable, body, .. } => {
            walk_expr(iterable, params, map, decl_params, mark);
            for s in body {
                walk_stmt(s, params, map, decl_params, mark);
            }
        }
        Stmt::While { cond, body, .. } => {
            walk_expr(cond, params, map, decl_params, mark);
            for s in body {
                walk_stmt(s, params, map, decl_params, mark);
            }
        }
        Stmt::Concurrent { body, .. } | Stmt::TierBlock { items: body, .. } => {
            for s in body {
                walk_stmt(s, params, map, decl_params, mark);
            }
        }
        // A nested `fn`'s own scope shadows/replaces the type-parameter scope; forwarded sites
        // inside it are not this function's (the checker rejects them there). Declarations carry
        // no forwarded expressions of ours.
        Stmt::Fn(_)
        | Stmt::Struct(_)
        | Stmt::Class(_)
        | Stmt::Enum(_)
        | Stmt::Trait(_)
        | Stmt::Impl(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. } => {}
    }
}

fn walk_expr(
    expr: &Expr,
    params: &[&str],
    map: &ForwardingMap,
    decl_params: &HashMap<&str, Vec<&str>>,
    mark: &mut dyn FnMut(&str, bool),
) {
    // Local shorthand: recurse with the same scope/context.
    macro_rules! rec {
        ($e:expr) => {
            walk_expr($e, params, map, decl_params, mark)
        };
    }
    match expr {
        // THE recipe consumer: a native call-site-typed turbofish naming a bare type parameter.
        Expr::TypedModuleCall { ty, args, .. } => {
            for p in params {
                if is_bare_param(ty, p) {
                    mark(p, true);
                }
            }
            for a in args {
                rec!(a);
            }
        }
        // The name-keyed manifest consumer.
        Expr::AttributesOf { ty, .. } => {
            for p in params {
                if is_bare_param(ty, p) {
                    mark(p, false);
                }
            }
        }
        // Transitive forwarding: an explicit turbofish call of another forwarding function whose
        // forwarding slot receives one of OUR bare parameters.
        Expr::TypedCall {
            name,
            type_args,
            args,
            ..
        } => {
            if let (Some(fwd), Some(callee_params)) =
                (map.get(name), decl_params.get(name.as_str()))
            {
                for fp in fwd {
                    if let Some(k) = callee_params.iter().position(|n| *n == fp.name)
                        && let Some(ta) = type_args.get(k)
                    {
                        for p in params {
                            if is_bare_param(ta, p) {
                                mark(p, fp.needs_recipe);
                            }
                        }
                    }
                }
            }
            for a in args {
                rec!(a);
            }
        }
        // A closure body runs within the enclosing generic's scope: forwarded sites inside it are
        // the enclosing function's (its hidden slot is captured like any local).
        Expr::Closure { body, .. } => match body {
            ClosureBody::Expr(e) => rec!(e),
            ClosureBody::Block(stmts) => {
                for s in stmts {
                    walk_stmt(s, params, map, decl_params, mark);
                }
            }
        },
        // Pure recursion over every other composite form.
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    rec!(e);
                }
            }
        }
        Expr::Unary { operand: e, .. }
        | Expr::Try { expr: e, .. }
        | Expr::Await { expr: e, .. }
        | Expr::Spawn { future: e, .. }
        | Expr::As { expr: e, .. }
        | Expr::TypeTest { expr: e, .. }
        | Expr::TypeOf { value: e, .. }
        | Expr::FieldsOf { value: e, .. }
        | Expr::ParamsOf { target: e, .. }
        | Expr::FromBytes { blob: e, .. }
        | Expr::Channel { capacity: e, .. }
        | Expr::TupleIndex { receiver: e, .. }
        | Expr::Member { receiver: e, .. } => rec!(e),
        Expr::Binary { lhs, rhs, .. } => {
            rec!(lhs);
            rec!(rhs);
        }
        Expr::Pipeline { left, right, .. } => {
            rec!(left);
            rec!(right);
        }
        Expr::Coalesce {
            value, fallback, ..
        } => {
            rec!(value);
            rec!(fallback);
        }
        Expr::Call { callee, args, .. } => {
            rec!(callee);
            for a in args {
                rec!(a);
            }
        }
        Expr::Index {
            receiver, index, ..
        } => {
            rec!(receiver);
            rec!(index);
        }
        Expr::Range { start, end, .. } => {
            rec!(start);
            rec!(end);
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            for i in items {
                rec!(i);
            }
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                rec!(k);
                rec!(v);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            rec!(scrutinee);
            for arm in arms {
                match &arm.body {
                    ClosureBody::Expr(e) => rec!(e),
                    ClosureBody::Block(stmts) => {
                        for s in stmts {
                            walk_stmt(s, params, map, decl_params, mark);
                        }
                    }
                }
            }
        }
        Expr::Object(lit) => {
            if let Some(spread) = &lit.spread {
                rec!(spread);
            }
            for f in &lit.fields {
                rec!(&f.value);
            }
        }
        Expr::Invoke {
            recv, name, args, ..
        } => {
            rec!(recv);
            rec!(name);
            rec!(args);
        }
        Expr::FieldSet {
            receiver, value, ..
        } => {
            rec!(receiver);
            rec!(value);
        }
        Expr::TierExpr { holes, .. } => {
            for h in holes {
                rec!(h);
            }
        }
        // Leaves.
        Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::IntN { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::Bool { .. }
        | Expr::Ident { .. }
        | Expr::RolesOf { .. }
        | Expr::NativeFnRef { .. } => {}
    }
}
