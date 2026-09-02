//! **Type-param forwarding pre-pass** (extended by poly-deferrals D2a): which
//! generic functions and methods forward a type parameter into a **call-site-typed position** — a
//! native turbofish (`json.try_parse::<T>`), a reflection manifest query (`attributes_of::<T>`),
//! the type's own name (`type_name::<T>()`), a **narrow** to it (`v.as<T>()` / `v is T`), the
//! **instantiation of a generic type constructed from it** (`Repository.new(…)` initializing a
//! `repo: Repository<T>`), or (transitively) another forwarding generic (`load::<T>(p)`).
//!
//! Generics are erased at runtime, so one compiled body serves every instantiation; a forwarded
//! site therefore needs its per-instantiation data (`TypeRecipe` / type name) delivered
//! **dynamically** — through the call node's own type-argument channel, indexing the program's
//! `TypeArgInfo` table. This pass computes, purely syntactically and BEFORE body checking, each
//! function's ordered list of forwarding **slots**, so both the body-side sites (which read the
//! slot) and the call sites (which supply it) agree on the layout.
//!
//! A slot is identified by its **type template** over the enclosing fn's type parameters — the
//! bare parameter (`T`) or a composite mentioning it (`List<T>`, `Map<string, T>`, `?T`,
//! `Result<T, E>`). The composite case is what makes `json.try_parse::<List<T>>` legal
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
//! Scope: **top-level `fn` declarations and methods.** A method is keyed `Type.method` and walked
//! over its OWN type parameters only — its class's parameters travel on a different channel (the
//! receiver's reflected type tag, which records an instantiation's name but no build recipe), so a
//! site naming one of those is a checker error rather than a slot. A method's slots reach it
//! through the call node's type-argument operand, never through prepended arguments: a method has
//! four name-keyed entry points with no static receiver type (a `dyn` receiver, either handle
//! form, `invoke`), all of which bind positionally, and a call through one of those supplies
//! nothing and aborts.
//!
//! A nested `fn` forwards the ENCLOSING body's parameters: it is walked with the enclosing
//! scope minus any names its own declaration shadows, and the slot it reads is the enclosing
//! function's (captured like any local by closure conversion). Transitive forwarding is recognized
//! through an EXPLICIT turbofish only (`g::<T>(x)`) — forwarding via argument inference alone is
//! rejected at the call site with a "spell the turbofish" help.

use crate::constructors::FreshConstructors;
use crate::env::{ParamSet, Subst, TypeScope};
use crate::subst::{
    apply_subst, bind_type_params, extend_param_scope, from_ref_q, mentions_param, param_ref,
    param_scope, scope_ids,
};
use noeta_ast::{ClosureBody, Expr, FnDecl, ObjectLit, Program, Stmt, StrPart, TypeRef};
use noeta_types::{ParamRef, Type};
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

/// How a call that must supply a hidden slot was **spelled** — the one thing the E0058 raised
/// when the enclosing body carries no matching slot needs in order to say something true.
///
/// This pass is syntactic, so what it could register a slot from is a property of the call's
/// spelling, not of its resolved callee: a turbofish on a **bare name** (`json.try_parse::<T>`,
/// `f::<T>(x)`, `s.load::<T>(x)`, `self.load::<T>(x)`) is one it sees; a turbofish through a
/// **compound receiver** (`self.inner.load::<T>(x)`) is one it cannot, because naming the callee
/// there means typing the receiver first. Both spellings arrive at the same checker seam, and the
/// diagnostic must not confuse them — the advice for one is a dead end for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardSpelling {
    /// A call with **no** explicit turbofish (`f(x)`, `recv.m(x)`): the instantiation was inferred
    /// from arguments or from an expected type. The pre-pass registers a transitive slot from an
    /// explicit turbofish only, so spelling one is the genuine fix here.
    Inferred,
    /// An explicit turbofish on a bare name — a spelling the pre-pass does see. Reaching the
    /// diagnostic anyway means the callee demands a slot template this body does not carry
    /// (the callee's own slot is a composite the turbofish type does not name).
    Turbofish,
    /// A method reached through a **compound receiver** (`self.inner.m::<T>(x)`, `f().m::<T>(x)`),
    /// whose owning type is a checking result. Binding the receiver to a local first turns it into
    /// the bare-name spelling above.
    CompoundReceiver,
}

/// The pre-pass result: the slot table, plus the functions whose slot set failed to converge
/// (polymorphic recursion through a composite forward — the checker reports these), plus the generic
/// types that read their own parameters reflectively.
pub(crate) struct Forwarding {
    pub(crate) map: ForwardingMap,
    pub(crate) poisoned: HashSet<String>,
    pub(crate) reflective: HashSet<String>,
}

/// Every **generic type that asks what one of its own type parameters is** — a member whose body
/// spells `type_name::<T>()`, `attributes_of::<T>()` or a native turbofish on a bare class parameter.
///
/// Such a type *needs* its instances tagged: the answer comes off the reflected type tag a
/// construction site stamped, so an untagged instance aborts at the first such query. A generic type
/// with no such member is indifferent to tagging, which is exactly why this set exists — it is the
/// guard that keeps [`crate::Checker::report_unrecordable`] from rejecting a working program that
/// merely happens to build a generic value at an unrecoverable instantiation and never reflect on it.
///
/// Computed with the same walk the slot fixpoint uses, over the class's parameters, so "a site that
/// consumes a bare parameter" means the identical thing in both — no second definition to drift.
/// Every member is walked, instance and self-less alike: which *channel* delivers the parameter is
/// the slot table's business, while this is only about whether the type asks at all.
fn reflective_generic_types<'a>(program: &'a Program, cx: &WalkCx<'a>) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for stmt in &program.stmts {
        let (name, type_params, methods) = match stmt {
            Stmt::Class(d) => (d.name.as_str(), &d.type_params, &d.methods),
            Stmt::Struct(d) => (d.name.as_str(), &d.type_params, &d.methods),
            Stmt::Enum(d) => (d.name.as_str(), &d.type_params, &d.methods),
            _ => continue,
        };
        if type_params.is_empty() {
            continue;
        }
        let own = param_scope(type_params, cx.xt);
        let own_ids = scope_ids(&own);
        for m in methods {
            let mut reads = false;
            {
                // Only a **bare** parameter counts: that is what the name-keyed reflective surfaces
                // consume, and what a missing tag makes unanswerable. A composite slot
                // (`List<T>`) belongs to the recipe channel, which is a different failure.
                let mut mark_fn = |template: Type, _: bool| {
                    if let Type::Param(p) = &template
                        && own_ids.contains(&p.id)
                    {
                        reads = true;
                    }
                };
                let mark: &mut dyn FnMut(Type, bool) = &mut mark_fn;
                let inner = WalkCx {
                    params: &own,
                    map: cx.map,
                    decl_params: cx.decl_params,
                    sigs: cx.sigs,
                    xt: cx.xt,
                    fresh: cx.fresh,
                    obj_fields: cx.obj_fields,
                    type_params: cx.type_params,
                    ret: m.ret.as_ref(),
                };
                for s in &m.body {
                    walk_stmt(s, &inner, mark);
                }
            }
            if reads {
                out.insert(name.to_string());
                break;
            }
        }
    }
    out
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
pub(crate) fn compute_forwarding(
    program: &Program,
    xt: &HashMap<String, String>,
    fresh: &FreshConstructors,
) -> Forwarding {
    let obj_fields = object_field_types(program);
    let decl_type_params = declared_type_params(program);
    let fns: Vec<&FnDecl> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Fn(f) if !f.type_params.is_empty() => Some(f),
            _ => None,
        })
        .collect();
    // Every candidate the fixpoint walks: the top-level generic `fn`s above, keyed by their bare
    // name, plus every generic METHOD, keyed `Type.method` — the same string `symbols.methods` and
    // the call site build, so a body's slots and its call sites agree on the layout.
    //
    // A method is walked over its own type parameters, plus — for a **self-less inherent member of
    // a generic type** — its class's (see [`forwardable_class_params`]). An *instance* method's
    // class parameters reach a reflected site through the other channel, the receiver's type tag
    // stamped at the construction site, so a site naming one of those is not this pass's business
    // and must not take a hidden slot. A self-less member has no receiver to read that tag from,
    // and this channel is the only one it has; a method parameter shadowing a class one is
    // therefore still correctly walked as the method's.
    let mut candidates: Vec<Candidate<'_>> = fns
        .iter()
        .map(|f| Candidate {
            key: f.name.to_string(),
            decl: f,
            own: param_scope(&f.type_params, xt),
        })
        .collect();
    for (ty, method, own) in method_candidates(program, xt) {
        candidates.push(Candidate {
            key: format!("{ty}.{}", method.name),
            decl: method,
            own,
        });
    }
    // The declaration-order type parameters of every candidate, for aligning turbofish arguments.
    let decl_params: HashMap<&str, Vec<ParamRef>> = fns
        .iter()
        .map(|f| {
            (
                f.name.as_str(),
                f.type_params.iter().map(param_ref).collect(),
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
            let scope = param_scope(&f.type_params, xt);
            let params: Vec<Type> = f
                .params
                .iter()
                .map(|p| {
                    p.ty.as_ref()
                        .map(|t| from_ref_q(t, xt, &scope))
                        .unwrap_or(Type::Unknown)
                })
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|t| from_ref_q(t, xt, &scope))
                .unwrap_or(Type::Unknown);
            (f.name.as_str(), (params, ret))
        })
        .collect();
    let mut map: ForwardingMap = HashMap::new();
    let mut poisoned: HashSet<String> = HashSet::new();
    loop {
        let mut changed = false;
        for c in &candidates {
            let f = c.decl;
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
                let cx = WalkCx {
                    params: &c.own,
                    map: &map,
                    decl_params: &decl_params,
                    sigs: &sigs,
                    xt,
                    fresh,
                    obj_fields: &obj_fields,
                    type_params: &decl_type_params,
                    ret: f.ret.as_ref(),
                };
                for stmt in &f.body {
                    walk_stmt(stmt, &cx, mark);
                }
                if overflow {
                    poisoned.insert(c.key.clone());
                }
            }
            if marks.is_empty() {
                continue;
            }
            if map.get(c.key.as_str()) != Some(&marks) {
                map.insert(c.key.clone(), marks);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // The reflective-type set rides along: it needs the same walk over a *type's* parameters, and
    // computing it here keeps "a site that consumes a bare parameter" one definition.
    let empty = TypeScope::new();
    let cx = WalkCx {
        params: &empty,
        map: &map,
        decl_params: &decl_params,
        sigs: &sigs,
        xt,
        fresh,
        obj_fields: &obj_fields,
        type_params: &decl_type_params,
        ret: None,
    };
    let reflective = reflective_generic_types(program, &cx);
    Forwarding {
        map,
        poisoned,
        reflective,
    }
}

/// One function the fixpoint walks: the key its slots are recorded under (a bare `fn` name, or
/// `Type.method`), the declaration whose body is walked, and the type parameters that body may
/// forward — its own, plus (for a self-less inherent member) its enclosing type's.
struct Candidate<'a> {
    key: String,
    decl: &'a FnDecl,
    own: TypeScope,
}

/// Every method the fixpoint walks, as `(owning type name, declaration, forwardable parameters)`.
/// Classes, structs and enums alike — all three flatten their `impl` blocks into `methods`, which is
/// also what the `(type, method)` dispatch machinery reads, so this sees exactly the methods a call
/// can reach. A method with nothing to forward is left out entirely.
fn method_candidates<'a>(
    program: &'a Program,
    xt: &HashMap<String, String>,
) -> Vec<(&'a str, &'a FnDecl, TypeScope)> {
    let mut out: Vec<(&str, &FnDecl, TypeScope)> = Vec::new();
    for stmt in &program.stmts {
        let (name, type_params, methods, impls) = match stmt {
            Stmt::Class(d) => (d.name.as_str(), &d.type_params, &d.methods, &d.impls),
            Stmt::Struct(d) => (d.name.as_str(), &d.type_params, &d.methods, &d.impls),
            Stmt::Enum(d) => (d.name.as_str(), &d.type_params, &d.methods, &d.impls),
            _ => continue,
        };
        // Which of `methods` came from an `impl Trait { … }` block (they are flattened in), keyed by
        // the one thing that identifies a declaration uniquely — its name span.
        let from_impl: HashSet<noeta_span::Span> = impls
            .iter()
            .flat_map(|b| b.methods.iter())
            .map(|m| m.name_span)
            .collect();
        for m in methods {
            // The class's forwardable parameters FIRST, then the method's own layered over them:
            // a method `<T>` shadowing a class `<T>` leaves only the method's in scope, which is
            // what `forwardable_class_params` used to say by filtering names out of a flat list.
            let own = extend_param_scope(
                &param_scope(
                    forwardable_class_params(type_params, m, from_impl.contains(&m.name_span)),
                    xt,
                ),
                &m.type_params,
                xt,
            );
            if !own.has_no_params() {
                out.push((name, m, own));
            }
        }
    }
    out
}

/// The **enclosing generic type's** parameters this member may forward through a hidden slot — its
/// class parameters when the member is one that has no other channel, and empty otherwise.
///
/// The split is the receiver. An *instance* method reads its class's instantiation off the
/// receiver's reflected type tag, stamped at the construction site, so its class parameters must NOT
/// take a hidden slot: two channels for one fact would let a call through one of the receiverless
/// entry points (a `dyn` receiver, a method handle, `invoke`) supply nothing and abort where the tag
/// was right there. A **self-less** member has no receiver at all — a generic type's constructor is
/// the motivating one, `fn new(tbl: string): Repo<T>`, where `T` is known only to the caller — and
/// the hidden slot is the only channel it has.
///
/// Two exclusions, both matching what `collect_method_sig_classified` classifies as
/// [`crate::Receiver::Static`], so the pre-pass and the checker cannot disagree about which
/// members have a receiver:
///
/// * a body that mentions `self` is an instance method, receiver channel;
/// * a member supplied by an `impl Trait { … }` block is reachable *either* way even when its body
///   is self-less (the trait's contract puts it in the instance interface, and `dyn Trait` dispatches
///   it on a value), so it keeps the receiver channel too.
///
/// A class parameter the method's own `<…>` shadows is a different type entirely and is dropped —
/// the method's own parameter is already in the list and owns the name.
fn forwardable_class_params<'a>(
    type_params: &'a [noeta_ast::TypeParam],
    m: &FnDecl,
    from_impl: bool,
) -> &'a [noeta_ast::TypeParam] {
    if type_params.is_empty() || from_impl || m.body.iter().any(|s| s.mentions("self")) {
        return &[];
    }
    // Shadowing is no longer a filter here: the method's own parameters are layered OVER these by
    // `extend_param_scope`, so a shadowed class parameter simply stops being reachable by name —
    // and the two `T`s are different parameters, so nothing else has to notice.
    type_params
}

/// Every declared struct/class's **field types by name**, for the generic-in-generic construction
/// consumer: an object literal's field initializer is a *checked* position, and the type it is
/// checked against is written on the field's declaration, so a fresh-constructor call there
/// instantiates whatever that declaration says.
type ObjectFieldTypes<'a> = HashMap<&'a str, HashMap<&'a str, &'a TypeRef>>;

/// Build [`ObjectFieldTypes`] over the program's structs and classes. Enums are absent: a variant
/// payload is positional, not a named field initializer.
fn object_field_types(program: &Program) -> ObjectFieldTypes<'_> {
    let mut out: ObjectFieldTypes<'_> = HashMap::new();
    for stmt in &program.stmts {
        let (name, fields) = match stmt {
            Stmt::Class(d) => (d.name.as_str(), &d.fields),
            Stmt::Struct(d) => (d.name.as_str(), &d.fields),
            _ => continue,
        };
        let entry = out.entry(name).or_default();
        for f in fields {
            if let Some(ty) = f.ty.as_ref() {
                entry.insert(f.name.as_str(), ty);
            }
        }
    }
    out
}

/// Every declared generic type's parameters, in declaration order — the alignment a nested
/// constructor's slot templates are substituted through.
fn declared_type_params(program: &Program) -> HashMap<&str, Vec<ParamRef>> {
    let mut out: HashMap<&str, Vec<ParamRef>> = HashMap::new();
    for stmt in &program.stmts {
        let (name, type_params) = match stmt {
            Stmt::Class(d) => (d.name.as_str(), &d.type_params),
            Stmt::Struct(d) => (d.name.as_str(), &d.type_params),
            Stmt::Enum(d) => (d.name.as_str(), &d.type_params),
            _ => continue,
        };
        if !type_params.is_empty() {
            out.insert(name, type_params.iter().map(param_ref).collect());
        }
    }
    out
}

/// The walk's read-only context: the enclosing fn's type parameters, the fixpoint state, every
/// candidate's declared parameter order, and the extern-type import map.
struct WalkCx<'a> {
    /// The type parameters the walked body may forward — a SCOPE, so a written `T` resolves to the
    /// parameter it names rather than merely being recognized as "some parameter".
    params: &'a TypeScope,
    map: &'a ForwardingMap,
    decl_params: &'a HashMap<&'a str, Vec<ParamRef>>,
    sigs: &'a HashMap<&'a str, (Vec<Type>, Type)>,
    xt: &'a HashMap<String, String>,
    /// The program's provable fresh constructors — the one call form whose reflected instantiation
    /// comes from its *position* rather than from its own arguments (see [`crate::constructors`]).
    fresh: &'a FreshConstructors,
    /// Declared field types, for the object-literal field-initializer position.
    obj_fields: &'a ObjectFieldTypes<'a>,
    /// Every declared generic **type**'s parameters in declaration order, for aligning a nested
    /// constructor's slot templates with the instantiation the position spells.
    type_params: &'a HashMap<&'a str, Vec<ParamRef>>,
    /// The walked declaration's declared **return** type, for the `return` position (and for
    /// resolving a target-typed `.{ … }` returned from it).
    ret: Option<&'a TypeRef>,
}

impl WalkCx<'_> {
    /// A surface type reference as the checker will see it, template-canonicalized.
    fn to_type(&self, ty: &TypeRef) -> Type {
        from_ref_q(ty, self.xt, self.params)
    }

    /// Whether a canonicalized type mentions one of the enclosing declaration's type parameters.
    fn mentions(&self, t: &Type) -> bool {
        mentions_param(t, &scope_ids(self.params))
    }

    /// The type `ty` names, when that is exactly one of the enclosing declaration's own **bare**
    /// parameters — the shape every name-keyed reflective consumer (`type_name`, `attributes_of`,
    /// a narrow) forwards for. `None` for anything else, a composite included: `List<T>` heads at
    /// `List`, a name no instantiation changes.
    fn bare_param(&self, ty: &TypeRef) -> Option<Type> {
        let t = self.to_type(ty);
        matches!(t, Type::Param(_)).then_some(t)
    }

    /// The **generic-in-generic construction** consumer: a fresh-constructor call of a generic type
    /// standing in a *checked* position whose declared type mentions one of the enclosing
    /// parameters. `Repository.new(tbl, pk)` initializing a field declared `repo: Repository<T>`
    /// inside `LiveRepository<T>` is the motivating one.
    ///
    /// The slot template is the **whole declared instantiation** (`Repository<T>`), not the bare
    /// parameter, and that is what makes this consumer cheap: the call site substitutes its own
    /// instantiation into the template and interns the *finished* `Repository<Todo>` statically,
    /// exactly as every other forwarding slot does — so the body's construction site stamps an
    /// already-interned `TypeRepr` chosen by one table index, and no template is ever composed at
    /// run time. It also composes for free: `Pair<int, T>` or `Repository<List<T>>` need nothing
    /// extra, and a body that also spells `type_name::<T>()` keeps that as its own separate slot.
    ///
    /// The head must match the constructor's own type, mirroring
    /// [`crate::Checker::note_constructor_call`]'s gate: a call whose result the position wraps
    /// (`items: List<Repository<T>>`) is not what this call constructs, and marking it would
    /// register a slot no body site ever reads while still obliging every caller to fill it.
    fn mark_ctor_position(
        &self,
        position: Option<&TypeRef>,
        value: &Expr,
        mark: &mut dyn FnMut(Type, bool),
    ) {
        let Some(position) = position else { return };
        let Some((ctor_ty, method)) = fresh_ctor_call_type(value, self.fresh) else {
            return;
        };
        let t = self.to_type(position);
        let Type::Named(n, args) = &t else { return };
        if n != ctor_ty || args.is_empty() || !self.mentions(&t) {
            return;
        }
        mark(t.clone(), false);
        // **Transitive** pass-through, one level of nesting at a time and therefore any depth: the
        // constructor we are calling may itself construct a generic out of ITS parameter, in which
        // case it carries a slot of its own that the call has to fill. Its template is over the
        // callee's class parameters, and this position's type arguments say what those are here
        // (`Outer<T>`'s `live: LiveRepository<T>` binds `LiveRepository`'s parameter to our `T`), so a
        // substituted template still mentioning ours becomes our slot — exactly the propagation the
        // explicit-turbofish call arm performs for a free function.
        //
        // A template mentioning the callee's OWN `<…>` parameters is deliberately left alone: those
        // are bound by argument inference, which this syntactic pass cannot see. The call site refuses
        // such a call outright rather than resolving it wrongly.
        let key = format!("{ctor_ty}.{method}");
        if let (Some(slots), Some(params)) = (self.map.get(&key), self.type_params.get(ctor_ty))
            && params.len() == args.len()
        {
            let subst: Subst = params
                .iter()
                .map(|p| p.id)
                .zip(args.iter().cloned())
                .collect();
            for slot in slots {
                let sigma = apply_subst(&slot.template, &subst);
                if self.mentions(&sigma) {
                    mark(sigma, slot.needs_recipe);
                }
            }
        }
    }

    /// Mark every field initializer of `lit` that is a fresh-constructor call, against the field's
    /// **declared** type. `elided` supplies the nominal type of a target-typed `.{ … }` (whose name
    /// the source omits) where the position it sits in makes it known.
    fn mark_object_positions(
        &self,
        lit: &ObjectLit,
        elided: Option<&str>,
        mark: &mut dyn FnMut(Type, bool),
    ) {
        let Some(name) = lit.type_name.as_ref().map(|n| n.as_str()).or(elided) else {
            return;
        };
        let Some(fields) = self.obj_fields.get(name) else {
            return;
        };
        for f in &lit.fields {
            self.mark_ctor_position(fields.get(f.name.as_str()).copied(), &f.value, mark);
        }
    }
}

/// The generic user type whose provable fresh constructor `expr` calls — the syntactic twin of
/// [`crate::Checker::fresh_constructor_type`], which answers the same question after name resolution.
/// `None` for every other expression.
fn fresh_ctor_call_type<'e>(
    expr: &'e Expr,
    fresh: &FreshConstructors,
) -> Option<(&'e str, &'e str)> {
    let Expr::Call { callee, .. } = expr else {
        return None;
    };
    let Expr::Member { receiver, name, .. } = callee.as_ref() else {
        return None;
    };
    // Peel a call-site instantiation: `Repo::<T>.new(…)` is the same fresh-constructor call as
    // `Repo.new(…)`, just with the instantiation written down.
    let Expr::Ident { name: tn, .. } = receiver.peel_instantiation() else {
        return None;
    };
    fresh
        .contains(&(tn.to_string(), name.to_string()))
        .then_some((tn.as_str(), name.as_str()))
}

/// The head name of a surface type reference, for resolving a target-typed `.{ … }` against the
/// position it stands in.
fn head_of(ty: &TypeRef) -> Option<&str> {
    match ty {
        TypeRef::Named { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// Whether a surface type reference is exactly the bare type parameter `param`.
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
                let tps: ParamSet = callee_params.iter().map(|p| p.id).collect();
                let mut subst: Subst = Subst::new();
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
            // An ANNOTATED binding is also a checked position for a generic type's fresh
            // constructor (`r: Repository<T> = Repository.new(…)`), and the one that gives a
            // construction the pre-pass cannot otherwise see — an argument, say — a spelling that
            // works: bind it here first, then pass the binding.
            cx.mark_ctor_position(Some(ann), value, mark);
            walk_expr(value, cx, mark)
        }
        Stmt::Binding { value, .. } => walk_expr(value, cx, mark),
        Stmt::Destructure { value, .. } => walk_expr(value, cx, mark),
        Stmt::Expr { expr, .. } => walk_expr(expr, cx, mark),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                // A declared `return` is a checked position: `fn repo(): Repository<T> { return
                // Repository.new(…); }`. A returned target-typed `.{ … }` takes its nominal type
                // from the same declaration, so its field initializers resolve too.
                cx.mark_ctor_position(cx.ret, v, mark);
                if let Expr::Object(lit) = v {
                    cx.mark_object_positions(lit, cx.ret.and_then(head_of), mark);
                }
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
        // A nested `fn` runs within the enclosing generic's type scope: forwarded sites
        // inside it consume the ENCLOSING fn's slots (the hidden slot is captured like any local
        // by closure conversion), so its body is walked with the enclosing parameters — minus any
        // the nested declaration's own type parameters shadow (those have no call-site channel;
        // the checker rejects sites naming them).
        Stmt::Fn(decl) => {
            // The nested declaration's own `<…>` REPLACE the enclosing entries they shadow, rather
            // than being filtered out of a flat name list: an inner `T` is a different parameter,
            // and a site naming it resolves to that one (which has no call-site channel, so the
            // checker rejects it) instead of silently reading the enclosing slot.
            let mut visible = extend_param_scope(cx.params, &decl.type_params, cx.xt);
            for p in &decl.type_params {
                visible.remove_param(&p.name);
            }
            if visible.is_empty() {
                return;
            }
            let inner = WalkCx {
                params: &visible,
                map: cx.map,
                decl_params: cx.decl_params,
                sigs: cx.sigs,
                xt: cx.xt,
                fresh: cx.fresh,
                obj_fields: cx.obj_fields,
                type_params: cx.type_params,
                // The nested declaration's own `return` position, not the enclosing one's.
                ret: decl.ret.as_ref(),
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
        // parameter — the bare `T` or a composite (`List<T>`). The whole turbofish type is
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
        // **The reflection surface, one arm.** Every name-keyed query is a NAME-ONLY consumer: it
        // keys a registry, a manifest or a narrow on the instantiation's runtime name, which is the
        // whole of what a slot's `TypeArgInfo.name` carries, so it forwards even for an
        // instantiation that has no build recipe at all. Their turbofish arm is
        // `field_specs_of(type_name::<T>())` with the composition done by the compiler rather than
        // by the author.
        //
        // Bare parameters only, matching the head-keyed identity these queries actually key on
        // ([`TypeRef::head_name`]): `field_specs_of::<List<T>>()` heads at `List`, a real type
        // whatever `T` is, and stays the compile-time constant it always was.
        //
        // `from_bytes` is the one exception, and it is the *shape* that says so rather than a name
        // listed here: it needs the element's packed layout, which no channel carries, so
        // `ReflectShape::StaticTypeWith::resolves_type_by_name` is false and its `T` does not
        // forward. That is a fact about the query, stated once, in front of the census.
        Expr::Reflect { which, operand, .. } => {
            if which.shape().resolves_type_by_name() {
                operand.for_each_type_ref(&mut |ty| {
                    if let Some(p) = cx.bare_param(ty) {
                        mark(p, false);
                    }
                });
            }
            operand.for_each_expr(&mut |e| walk_expr(e, cx, mark));
        }
        // The **narrow** consumers — `v.as<T>()` and `v is T`. Name-only, like `type_name` above
        // and for the same reason: a narrow is a head-constructor match on the instantiation's
        // runtime NAME ([`noeta_ast::Expr::As`]), so a slot's `TypeArgInfo.name` is the whole of
        // what it reads and it forwards even for an instantiation that has no recipe at all.
        //
        // Bare parameters only, matching the head-keyed match the runtime performs: one narrow
        // reads one name, so a parameter buried in a *tested* argument position (`v is List<T>`)
        // has no slot it could be spelled as and is an E0058 at the checker instead.
        Expr::As { expr: e, ty, .. } | Expr::TypeTest { expr: e, ty, .. } => {
            if let Some(p) = cx.bare_param(ty) {
                mark(p, false);
            }
            rec!(e);
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
                let subst: Subst = callee_params
                    .iter()
                    .map(|p| p.id)
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
        // A generic method's own parameters DO forward now (Axis A), but transitive forwarding
        // through a member call is not recognized here: this pass is purely syntactic, and the
        // receiver's type — which is what would name the callee's slot layout — is a checking
        // result, not a syntactic one. A site that needs it is refused at the call rather than
        // resolved wrongly. Receiver and arguments recurse like any call's.
        Expr::TypedMethodCall { recv, args, .. } => {
            // `Repo::<T>.make::<U>(…)` — BOTH turbofishes. The class half is the same checked
            // construction position the single-turbofish `Repo::<T>.new(…)` is (that one arrives
            // as `Expr::Call { callee: Member { receiver: InstantiatedType } }`, this one as a
            // `TypedMethodCall` whose `recv` is the `InstantiatedType`), so it feeds the same
            // consumer with the same template. Without this the two spellings would disagree about
            // whether a construction out of an enclosing parameter is recordable.
            if let Expr::InstantiatedType {
                recv: head,
                type_args,
                span,
            } = recv.as_ref()
                && let Expr::Ident { name, .. } = head.as_ref()
            {
                let position = TypeRef::Named {
                    name: name.clone(),
                    args: type_args.clone(),
                    span: *span,
                };
                cx.mark_ctor_position(Some(&position), expr, mark);
            }
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
            // `Repo::<T>.new(…)` — a call-site instantiation is a **checked position** in exactly
            // the sense [`WalkCx::mark_ctor_position`] means, and the most direct one there is: the
            // site spells the whole instantiation itself rather than having it read off a declared
            // field/return/binding. So it feeds the same consumer with the same template
            // (`Repo<T>`), and a self-less member of a generic can construct out of its own
            // parameter with the type written at the call.
            //
            // A concrete instantiation (`Repo::<Todo>`) mentions no enclosing parameter and
            // `mark_ctor_position` drops it — no slot, nothing forwarded, exactly as for a
            // concrete field type.
            if let Expr::Member { receiver, .. } = callee.as_ref()
                && let Expr::InstantiatedType {
                    recv,
                    type_args,
                    span,
                } = receiver.as_ref()
                && let Expr::Ident { name, .. } = recv.as_ref()
            {
                let position = TypeRef::Named {
                    name: name.clone(),
                    args: type_args.clone(),
                    span: *span,
                };
                cx.mark_ctor_position(Some(&position), expr, mark);
            }
            // `T.m(…)` — a bare enclosing parameter in RECEIVER position. Name-only, exactly like
            // `type_name::<T>()` and the narrows above and for the same reason: the call is
            // rewritten into the runtime's by-name dispatch, so the instantiation's name is the
            // whole of what it reads. Without this the slot is never allocated and the body has no
            // channel to resolve `T` through, which is the E0058 the checker would then report.
            //
            // The receiver is a bare identifier that RESOLVES to a parameter, which is as much as
            // this walk can know — it runs before name resolution, so a local variable sharing a
            // parameter's spelling marks a slot no site reads. That costs a hidden slot on a
            // function whose author spelled a value `T`; it cannot make a call wrong, since the
            // checker records the site only where the name really is the parameter.
            if let Expr::Member { receiver, .. } = callee.as_ref()
                && let Expr::Ident { name, span } = receiver.as_ref()
            {
                let as_ref = TypeRef::Named {
                    name: name.clone(),
                    args: Vec::new(),
                    span: *span,
                };
                if let Some(p) = cx.bare_param(&as_ref) {
                    mark(p, false);
                }
            }
            rec!(callee);
            for a in noeta_ast::CallArg::values(args) {
                rec!(a);
            }
        }
        // The instantiation itself carries only types; the reference under it recurses like any
        // other receiver.
        Expr::InstantiatedType { recv, .. } => rec!(recv),
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
            // A field initializer is a checked position (see [`WalkCx::mark_ctor_position`]) — the
            // motivating one for generic-in-generic construction, since a generic type's constructor
            // builds its inner generic right there in the literal it returns.
            cx.mark_object_positions(lit, None, mark);
            if let Some(spread) = &lit.spread {
                rec!(spread);
            }
            for f in &lit.fields {
                rec!(&f.value);
            }
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
        | Expr::NativeFnRef { .. } => {}
    }
}
