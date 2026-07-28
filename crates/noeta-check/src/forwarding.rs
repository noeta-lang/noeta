//! **Type-param forwarding pre-pass** (poly-values F2b, extended by poly-deferrals D2a): which
//! top-level generic functions forward a type parameter into a **call-site-typed position** — a
//! native turbofish (`json.try_parse::<T>`), a reflection manifest query (`attributes_of::<T>`),
//! or (transitively) another forwarding generic (`load::<T>(p)`).
//!
//! Generics are erased at runtime, so one compiled body serves every instantiation; a forwarded
//! site therefore needs its per-instantiation data (`TypeRecipe` / type name) delivered
//! **dynamically** — as a hidden call argument indexing the program's `TypeArgInfo` table. This
//! pass computes, purely syntactically and BEFORE body checking, each function's ordered list of
//! forwarding **slots**, so both the body-side sites (which read the hidden slot) and the call
//! sites (which supply it) agree on the layout.
//!
//! A slot is identified by its **type template** over the enclosing fn's type parameters — the
//! bare parameter (`T`) or a composite mentioning it (`List<T>`, `Map<string, T>`, `?T`,
//! `Result<T, E>`). The composite case (D2a) is what makes `json.try_parse::<List<T>>` legal
//! inside a generic body: the CALL SITE substitutes its concrete instantiation into the template
//! (`T = Order` → `List<Order>`) and interns that whole concrete type into the program-wide
//! `TypeArgInfo` table — statically, so the runtime never constructs a recipe. A call that
//! forwards the caller's own parameter passes the caller's matching slot through instead
//! (`HiddenArg::Forward`), which is why the templates propagate through the call graph by
//! substitution here (a fixpoint, as before).
//!
//! Substitution-propagated templates can GROW (`f<T>` calling `f::<List<T>>` would demand
//! `List<T>`, then `List<List<T>>`, …): genuine polymorphic recursion, which erased generics
//! with a static table cannot serve. The fixpoint caps template depth and reports the offending
//! function (`poisoned`) instead of looping — the checker turns that into a clear E0058.
//!
//! Scope: **top-level `fn` declarations only.** Methods carry their class's parameters (a
//! different instantiation channel); a forwarded site there is a checker error, not silently
//! wrong. A nested `fn` forwards the ENCLOSING top-level fn's parameters (D2b): its body is
//! walked with the enclosing scope minus any names its own declaration shadows, and the slot it
//! reads is the enclosing fn's (captured like any local by closure conversion). Transitive
//! forwarding is recognized through an EXPLICIT turbofish only (`g::<T>(x)`) — forwarding via
//! argument inference alone is rejected at the call site with a "spell the turbofish" help.

use crate::subst::{apply_subst, bind_type_params, from_ref_q, mentions_param};
use noeta_ast::{ClosureBody, Expr, FnDecl, Program, Stmt, StrPart, TypeRef};
use noeta_types::Type;
use std::collections::{HashMap, HashSet};

/// One forwarding slot of a generic function, in first-appearance order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardSlot {
    /// The slot's type template over the fn's type parameters: the bare parameter (`T`) or a
    /// composite mentioning it (`List<T>`). A call site substitutes its instantiation into this
    /// template to produce the concrete type the hidden argument indexes.
    pub(crate) template: Type,
    /// Whether some forwarded site consumes a **build recipe** (a `TypedModuleCall` turbofish) —
    /// if so, an instantiating call must supply a recipe-capable type (checked statically at the
    /// call site). A name-only consumer (`attributes_of`) leaves this `false`.
    pub(crate) needs_recipe: bool,
}

/// The per-function forwarding table: fn name → its forwarding slots, in first-appearance order.
pub(crate) type ForwardingMap = HashMap<String, Vec<ForwardSlot>>;

/// The pre-pass result: the slot table, plus the functions whose slot set failed to converge
/// (polymorphic recursion through a composite forward — the checker reports these).
pub(crate) struct Forwarding {
    pub(crate) map: ForwardingMap,
    pub(crate) poisoned: HashSet<String>,
}

/// The deepest template the fixpoint will register. Substitution-grown templates past this depth
/// mean the slot set is diverging (polymorphic recursion); real programs nest a handful at most.
const MAX_TEMPLATE_DEPTH: usize = 8;

/// Structural nesting depth of a type (a bare name is 1, `List<T>` is 2, …) — the fixpoint's
/// divergence measure.
fn type_depth(t: &Type) -> usize {
    let inner = match t {
        Type::Named(_, args) => args.iter().map(type_depth).max().unwrap_or(0),
        Type::List(e) | Type::Set(e) | Type::Option(e) => type_depth(e),
        Type::Map(k, v) | Type::Result(k, v) => type_depth(k).max(type_depth(v)),
        Type::Tuple(es) | Type::Union(es) => es.iter().map(type_depth).max().unwrap_or(0),
        Type::Fn { params, ret } => params
            .iter()
            .map(type_depth)
            .max()
            .unwrap_or(0)
            .max(type_depth(ret)),
        _ => 0,
    };
    inner + 1
}

/// Compute the program's forwarding table — a fixpoint over the top-level `fn`s (a function
/// forwards transitively through a turbofish call of another forwarding function). `xt` is the
/// program's extern-type import map, so a template's non-parameter names carry the same
/// qualified identity the checker's body-site and call-site types do.
pub(crate) fn compute_forwarding(program: &Program, xt: &HashMap<String, String>) -> Forwarding {
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
    // Each candidate's raw declared signature (parameter/return types over its own parameters),
    // for binding an ANNOTATED VALUE BINDING of a forwarding fn (D2c pass-through:
    // `d: (string) -> Result<T, E> = load` inside another generic — the annotation instantiates
    // the callee, and any slot still mentioning OUR parameters becomes our slot).
    let sigs: HashMap<&str, (Vec<Type>, Type)> = fns
        .iter()
        .map(|f| {
            let params: Vec<Type> = f
                .params
                .iter()
                .map(|p| {
                    p.ty.as_ref()
                        .map(|t| from_ref_q(t, xt))
                        .unwrap_or(Type::Unknown)
                })
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|t| from_ref_q(t, xt))
                .unwrap_or(Type::Unknown);
            (f.name.as_str(), (params, ret))
        })
        .collect();
    let mut map: ForwardingMap = HashMap::new();
    let mut poisoned: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for f in &fns {
            // Collect this pass's marks in first-appearance order.
            let mut marks: Vec<ForwardSlot> = Vec::new();
            {
                let mut overflow = false;
                let mut mark_fn = |template: Type, needs_recipe: bool| {
                    if type_depth(&template) > MAX_TEMPLATE_DEPTH {
                        overflow = true;
                        return;
                    }
                    match marks.iter_mut().find(|s| s.template == template) {
                        Some(slot) => slot.needs_recipe |= needs_recipe,
                        None => marks.push(ForwardSlot {
                            template,
                            needs_recipe,
                        }),
                    }
                };
                let mark: &mut dyn FnMut(Type, bool) = &mut mark_fn;
                let params: Vec<&str> = f.type_params.iter().map(|p| p.name.as_str()).collect();
                let cx = WalkCx {
                    params: &params,
                    map: &map,
                    decl_params: &decl_params,
                    sigs: &sigs,
                    xt,
                };
                for stmt in &f.body {
                    walk_stmt(stmt, &cx, mark);
                }
                if overflow {
                    poisoned.insert(f.name.to_string());
                }
            }
            if marks.is_empty() {
                continue;
            }
            if map.get(f.name.as_str()) != Some(&marks) {
                map.insert(f.name.to_string(), marks);
                changed = true;
            }
        }
        if !changed {
            return Forwarding { map, poisoned };
        }
    }
}

/// The walk's read-only context: the enclosing fn's type parameters, the fixpoint state, every
/// candidate's declared parameter order, and the extern-type import map.
struct WalkCx<'a> {
    params: &'a [&'a str],
    map: &'a ForwardingMap,
    decl_params: &'a HashMap<&'a str, Vec<&'a str>>,
    sigs: &'a HashMap<&'a str, (Vec<Type>, Type)>,
    xt: &'a HashMap<String, String>,
}

impl WalkCx<'_> {
    /// A surface type reference as the checker will see it, template-canonicalized.
    fn to_type(&self, ty: &TypeRef) -> Type {
        from_ref_q(ty, self.xt)
    }

    /// Whether a canonicalized type mentions one of the enclosing fn's type parameters.
    fn mentions(&self, t: &Type) -> bool {
        let params: Vec<String> = self.params.iter().map(|p| p.to_string()).collect();
        mentions_param(t, &params)
    }
}

/// Whether a surface type reference is exactly the bare type parameter `param`.
fn is_bare_param(ty: &TypeRef, param: &str) -> bool {
    matches!(ty, TypeRef::Named { name, args, .. } if name == param && args.is_empty())
}

fn walk_stmt(stmt: &Stmt, cx: &WalkCx<'_>, mark: &mut dyn FnMut(Type, bool)) {
    match stmt {
        Stmt::Echo { value: e, .. } | Stmt::Yield { value: e, .. } => walk_expr(e, cx, mark),
        // An ANNOTATED binding of a forwarding fn as a VALUE (D2c pass-through): the annotation
        // instantiates the callee's signature; a slot the substitution leaves mentioning OUR
        // parameters (`d: (string) -> Result<T, E> = load`) becomes our slot — the same
        // propagation an explicit turbofish call performs.
        Stmt::Binding {
            ty: Some(ann),
            value,
            ..
        } => {
            if let Expr::Ident { name, .. } = value
                && let (Some(slots), Some(callee_params), Some((raw_params, raw_ret))) = (
                    cx.map.get(name.as_str()),
                    cx.decl_params.get(name.as_str()),
                    cx.sigs.get(name.as_str()),
                )
                && let Type::Fn {
                    params: ann_params,
                    ret: ann_ret,
                } = cx.to_type(ann)
            {
                let tps: HashSet<String> = callee_params.iter().map(|p| p.to_string()).collect();
                let mut subst: HashMap<String, Type> = HashMap::new();
                for (raw, exp) in raw_params.iter().zip(&ann_params) {
                    bind_type_params(raw, exp, &tps, &mut subst);
                }
                bind_type_params(raw_ret, &ann_ret, &tps, &mut subst);
                for slot in slots {
                    let sigma = apply_subst(&slot.template, &subst);
                    if cx.mentions(&sigma) {
                        mark(sigma, slot.needs_recipe);
                    }
                }
            }
            walk_expr(value, cx, mark)
        }
        Stmt::Binding { value, .. } => walk_expr(value, cx, mark),
        Stmt::Destructure { value, .. } => walk_expr(value, cx, mark),
        Stmt::Expr { expr, .. } => walk_expr(expr, cx, mark),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                walk_expr(v, cx, mark);
            }
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            walk_expr(cond, cx, mark);
            for s in then_body {
                walk_stmt(s, cx, mark);
            }
            if let Some(b) = else_body {
                for s in b {
                    walk_stmt(s, cx, mark);
                }
            }
        }
        Stmt::For { iterable, body, .. } => {
            walk_expr(iterable, cx, mark);
            for s in body {
                walk_stmt(s, cx, mark);
            }
        }
        Stmt::While { cond, body, .. } => {
            walk_expr(cond, cx, mark);
            for s in body {
                walk_stmt(s, cx, mark);
            }
        }
        Stmt::Concurrent { body, .. } | Stmt::TierBlock { items: body, .. } => {
            for s in body {
                walk_stmt(s, cx, mark);
            }
        }
        // A nested `fn` runs within the enclosing generic's type scope (D2b): forwarded sites
        // inside it consume the ENCLOSING fn's slots (the hidden slot is captured like any local
        // by closure conversion), so its body is walked with the enclosing parameters — minus any
        // the nested declaration's own type parameters shadow (those have no call-site channel;
        // the checker rejects sites naming them).
        Stmt::Fn(decl) => {
            let shadowed: Vec<&str> = decl.type_params.iter().map(|p| p.name.as_str()).collect();
            let visible: Vec<&str> = cx
                .params
                .iter()
                .copied()
                .filter(|p| !shadowed.contains(p))
                .collect();
            if visible.is_empty() {
                return;
            }
            let inner = WalkCx {
                params: &visible,
                map: cx.map,
                decl_params: cx.decl_params,
                sigs: cx.sigs,
                xt: cx.xt,
            };
            for s in &decl.body {
                walk_stmt(s, &inner, mark);
            }
        }
        // Declarations carry no forwarded expressions of ours.
        Stmt::Struct(_)
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

fn walk_expr(expr: &Expr, cx: &WalkCx<'_>, mark: &mut dyn FnMut(Type, bool)) {
    // Local shorthand: recurse with the same scope/context.
    macro_rules! rec {
        ($e:expr) => {
            walk_expr($e, cx, mark)
        };
    }
    match expr {
        // THE recipe consumer: a native call-site-typed turbofish whose type mentions an enclosing
        // parameter — the bare `T` or a composite (`List<T>`, D2a). The whole turbofish type is
        // the slot template.
        Expr::TypedModuleCall { ty, args, .. } => {
            let t = cx.to_type(ty);
            if cx.mentions(&t) {
                mark(t, true);
            }
            for a in noeta_ast::CallArg::values(args) {
                rec!(a);
            }
        }
        // The name-keyed manifest consumer: bare parameters only (an attribute type is a bare
        // struct name by construction).
        Expr::AttributesOf { ty, .. } => {
            for p in cx.params {
                if is_bare_param(ty, p) {
                    mark(Type::Named(p.to_string(), Vec::new()), false);
                }
            }
        }
        // Transitive forwarding: an explicit turbofish call of another forwarding function. Each
        // of the callee's slot templates, with the call's type arguments substituted in, that
        // still mentions one of OUR parameters becomes our slot (`g::<T>` against g's `List<U>`
        // slot demands our `List<T>`).
        Expr::TypedCall {
            name,
            type_args,
            args,
            ..
        } => {
            if let (Some(slots), Some(callee_params)) =
                (cx.map.get(name.as_str()), cx.decl_params.get(name.as_str()))
                && type_args.len() == callee_params.len()
            {
                let subst: HashMap<String, Type> = callee_params
                    .iter()
                    .map(|p| p.to_string())
                    .zip(type_args.iter().map(|t| cx.to_type(t)))
                    .collect();
                for slot in slots {
                    let sigma = apply_subst(&slot.template, &subst);
                    if cx.mentions(&sigma) {
                        mark(sigma, slot.needs_recipe);
                    }
                }
            }
            for a in noeta_ast::CallArg::values(args) {
                rec!(a);
            }
        }
        // A generic METHOD's own type parameters never forward (the pinned D3 boundary — method
        // dispatch has no hidden-slot channel), so a member-call turbofish contributes nothing;
        // its receiver/arguments recurse like any call's.
        Expr::TypedMethodCall { recv, args, .. } => {
            rec!(recv);
            for a in noeta_ast::CallArg::values(args) {
                rec!(a);
            }
        }
        // A closure body runs within the enclosing generic's scope: forwarded sites inside it are
        // the enclosing function's (its hidden slot is captured like any local).
        Expr::Closure { body, .. } => match body {
            ClosureBody::Expr(e) => rec!(e),
            ClosureBody::Block(stmts) => {
                for s in stmts {
                    walk_stmt(s, cx, mark);
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
        | Expr::TraitsOf { value: e, .. }
        | Expr::ParamsOf { target: e, .. }
        | Expr::ReturnsOf { target: e, .. }
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
            for a in noeta_ast::CallArg::values(args) {
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
                            walk_stmt(s, cx, mark);
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
            if let Some(recv) = recv {
                rec!(recv);
            }
            rec!(name);
            rec!(args);
        }
        // A turbofish operand names a *declared* type, never an enclosing type parameter's slot (a
        // `T` there is erased and reflects as nothing), so it forwards no recipe.
        Expr::FieldSpecsOf { name, .. } => {
            if let Some(e) = name.dynamic() {
                rec!(e);
            }
        }
        Expr::Construct { name, fields, .. } => {
            if let Some(e) = name.dynamic() {
                rec!(e);
            }
            rec!(fields);
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
        // Leaves. `type_name::<T>()` is one on purpose: it is NOT a forwarding consumer, because a
        // type parameter there is rejected outright (E0058) rather than resolved through a hidden
        // slot — the string it yields is a compile-time constant, with no runtime node to feed.
        Expr::TypeName { .. }
        | Expr::Str { .. }
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
