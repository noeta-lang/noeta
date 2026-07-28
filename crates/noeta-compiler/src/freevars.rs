//! Free-variable analysis for closure conversion, over the **Core IR**.
//!
//! A closure or nested `fn` may reference a binding from an enclosing function; the VM models
//! that as an *upvalue* (a captured cell). To lower a function the compiler must know, before
//! it emits code, which of its own locals an inner closure captures (those become cells) and
//! which enclosing-function names the function itself captures (those become its upvalues).
//! Both fall out of one question: **which names does a function body reference that resolve to
//! a binding in an enclosing function?** — its *free variables*.
//!
//! The analysis runs on the lowered IR, which is *simpler* than the surface AST for this
//! purpose: every operand is an [`Atom`], so only an [`Atom::Var`] contributes a reference
//! (temporaries and constants never do); `|>` is gone (lowered to ordinary calls); and
//! short-circuit operators are explicit [`Stmt::Logical`]/[`Stmt::Coalesce`] blocks. Source
//! variable *names* survive lowering unchanged, so the locality rules are identical to the
//! surface analysis they replace.
//!
//! The one wrinkle is the language's bare-assignment rule (matching the tree-walker's
//! `Scope::assign`, which searches outward): a bare `x = v` *reassigns* an enclosing binding if
//! one exists, and only declares a fresh local when the name is found nowhere outer. So locality
//! is context-sensitive — a name is local to a function only if it is `mut`/param/for/`fn`/match-
//! bound, or bare-assigned **and** absent from every enclosing function scope and the globals.
//! The analysis therefore threads the enclosing function locals (capturable) and the module
//! globals (not capturable) down through nesting.

use std::collections::{BTreeSet, HashSet};

use noeta_ir::{Atom, Block, Decl, ForPattern, Pattern, Rvalue, Stmt, Thunk};

/// The names a function body references that resolve to a binding in one of `enclosing_locals`
/// (the enclosing functions' locals, outermost first) — i.e. the function's captured upvalues.
/// `globals` is consulted only to decide bare-assignment locality (a bare assign to a global is
/// a reassignment, not a new local), never reported as free.
pub fn free_vars(
    params: &[String],
    defaults: &[Option<Thunk>],
    body: &Block,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
    captures: Option<&[String]>,
) -> BTreeSet<String> {
    let local = local_names(params, body, enclosing_locals, globals, captures);

    let mut enclosing_any: HashSet<String> = HashSet::new();
    for scope in enclosing_locals {
        enclosing_any.extend(scope.iter().cloned());
    }

    // Every referenced name, including those bubbled up from nested closures (computed against
    // this function's locals as a new enclosing layer).
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let inner_enclosing = push_layer(enclosing_locals, local.clone());
    collect_refs_block(body, &inner_enclosing, globals, &mut referenced);
    // A parameter's default value is evaluated in the function's *definition* scope, not against
    // its own parameters/locals, so its references are collected one layer out (`enclosing_locals`,
    // not `inner_enclosing`). This is what lets a closure capture a variable used only by a default
    // (e.g. `fn(x, step = base) => ...` where the body never names `base`): the captured `base`
    // must be in the closure's upvalue set for the VM's default thunk to read it.
    for default in defaults.iter().flatten() {
        collect_refs_block(&default.body, enclosing_locals, globals, &mut referenced);
    }

    referenced
        .into_iter()
        .filter(|n| enclosing_any.contains(n) && !local.contains(n))
        .collect()
}

/// `enclosing_locals` with `layer` appended — the enclosing chain seen from inside this function.
fn push_layer(
    enclosing_locals: &[HashSet<String>],
    layer: HashSet<String>,
) -> Vec<HashSet<String>> {
    let mut chain = enclosing_locals.to_vec();
    chain.push(layer);
    chain
}

/// What the compiler needs to lower a function: its own local names (so child captures can be
/// sourced and bare-assignment locality decided) and the subset of those locals that an inner
/// closure captures (which must therefore be stored as cells).
pub struct Analysis {
    pub local: HashSet<String>,
    pub celled: HashSet<String>,
}

/// Compute a function's [`Analysis`]. `celled` is the function's locals that appear free in some
/// nested closure/`fn` — exactly the locals that must live in cells so the capture is shared.
pub fn analyze(
    params: &[String],
    defaults: &[Option<Thunk>],
    body: &Block,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
    captures: Option<&[String]>,
) -> Analysis {
    let local = local_names(params, body, enclosing_locals, globals, captures);
    let inner_enclosing = push_layer(enclosing_locals, local.clone());
    let mut nested: BTreeSet<String> = BTreeSet::new();
    collect_nested_frees_block(body, &inner_enclosing, globals, &mut nested);
    // A default thunk is itself a sub-scope that may close over a function local; its captures, if
    // local here, must be celled too. Defaults run in the definition scope, so they are analyzed
    // one layer out (like `free_vars`).
    for default in defaults.iter().flatten() {
        collect_nested_frees_block(&default.body, enclosing_locals, globals, &mut nested);
    }
    let celled = nested.into_iter().filter(|n| local.contains(n)).collect();
    Analysis { local, celled }
}

// --- The function's own local bindings ---------------------------------------------------------

/// The names this function binds as its own locals (params, `mut`/`fn`/for/match bindings, and
/// bare assignments that do not reach an outer binding). Nested closures are opaque — their
/// bindings belong to them, not here.
fn local_names(
    params: &[String],
    body: &Block,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
    captures: Option<&[String]>,
) -> HashSet<String> {
    // A SEALED fn (named, `captures = Some(allow)`): a bare assignment reaches a surrounding
    // binding only through the `use (…)` allow-list — every other bare-assigned name is a fresh
    // local, exactly as the checker typed it. An auto-capturing closure (`None`) keeps the full
    // outward rule. Reads are unaffected either way (free-variable capture and global loads are
    // computed from references, not from this locality set).
    // Synthesized state-machine variables (`$state`, …, unspellable by users) always follow the
    // auto rule — the async/generator step closure inherits its fn's seal, but its own machinery
    // must keep reassigning the captured state cells.
    let mut outer: HashSet<String> = match captures {
        Some(allow) => allow
            .iter()
            .cloned()
            .chain(synthesized_names(body))
            .collect(),
        None => {
            let mut outer = globals.clone();
            for scope in enclosing_locals {
                outer.extend(scope.iter().cloned());
            }
            outer
        }
    };
    // Harmless but principled: an allow-listed name that is neither global nor enclosing would
    // never resolve anyway; keep the set as-is (no filtering) so behavior is order-independent.
    let _ = &mut outer;
    let mut local: HashSet<String> = params.iter().cloned().collect();
    collect_bindings_block(body, &outer, &mut local);
    local
}

/// Every `$`-prefixed name a body assigns — the lowering's synthesized state-machine variables,
/// which are seal-exempt (see `local_names`).
fn synthesized_names(body: &Block) -> Vec<String> {
    let mut all: HashSet<String> = HashSet::new();
    collect_bindings_block(body, &HashSet::new(), &mut all);
    all.into_iter().filter(|n| n.starts_with('$')).collect()
}

/// Bindings a block introduces into the current function (recursing into every sub-block and
/// match arm, but not into nested closures/`fn`s — those own their bindings). `outer` is every
/// enclosing-or-global name, used to decide whether a bare assignment is a fresh local.
fn collect_bindings_block(block: &Block, outer: &HashSet<String>, local: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_bindings_stmt(stmt, outer, local);
    }
}

fn collect_bindings_stmt(stmt: &Stmt, outer: &HashSet<String>, local: &mut HashSet<String>) {
    match stmt {
        Stmt::Bind { mut_decl, name, .. } => {
            if *mut_decl || !outer.contains(name) {
                local.insert(name.clone());
            }
        }
        Stmt::If {
            then_block,
            else_block,
            ..
        } => {
            collect_bindings_block(then_block, outer, local);
            if let Some(else_block) = else_block {
                collect_bindings_block(else_block, outer, local);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_bindings_block(cond, outer, local);
            collect_bindings_block(body, outer, local);
        }
        Stmt::For { pattern, body, .. } => {
            match pattern {
                ForPattern::Single { name, .. } => {
                    local.insert(name.clone());
                }
                // Desugared to a `Single` hidden var in lowering, so unreachable in the IR; kept for
                // totality.
                ForPattern::Tuple { names, .. } => {
                    for (name, _) in names {
                        local.insert(name.clone());
                    }
                }
            }
            collect_bindings_block(body, outer, local);
        }
        Stmt::Match { arms, .. } => {
            for arm in arms {
                pattern_names(&arm.pattern, local);
                if let Some(guard) = &arm.guard {
                    collect_bindings_block(&guard.block, outer, local);
                }
                collect_bindings_block(&arm.body, outer, local);
            }
        }
        Stmt::Logical { right, .. } => collect_bindings_block(right, outer, local),
        Stmt::Coalesce { fallback, .. } => collect_bindings_block(fallback, outer, local),
        Stmt::Decl(Decl::Fn { name, .. }) => {
            // A nested `fn` binds its own name as a local here; its body is opaque (own scope).
            local.insert(name.clone());
        }
        // `let`/`eval` only bind temporaries (not source names); their rvalues introduce no source
        // bindings (a nested closure is opaque). Every other statement binds nothing.
        Stmt::Let { .. }
        | Stmt::Eval { .. }
        | Stmt::Echo { .. }
        | Stmt::Return { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. }
        | Stmt::Decl(_) => {}
    }
}

/// The names a pattern binds (recursing into nested variant sub-patterns).
fn pattern_names(pattern: &Pattern, out: &mut HashSet<String>) {
    match pattern {
        Pattern::Binding { name, .. } => {
            out.insert(name.clone());
        }
        Pattern::Variant { bindings, .. } => {
            for sub in bindings {
                pattern_names(sub, out);
            }
        }
        Pattern::Tuple { elements, .. } => {
            for sub in elements {
                pattern_names(sub, out);
            }
        }
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. }
        | Pattern::IsType { .. } => {}
    }
}

// --- Every name a body references --------------------------------------------------------------

fn collect_refs_block(
    block: &Block,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    for stmt in &block.stmts {
        collect_refs_stmt(stmt, enclosing, globals, out);
    }
    if let Some(tail) = &block.tail {
        atom_ref(tail, out);
    }
}

/// Insert a [`Atom::Var`]'s name as a reference; temporaries and constants reference nothing.
fn atom_ref(atom: &Atom, out: &mut BTreeSet<String>) {
    if let Atom::Var { name, .. } = atom {
        out.insert(name.clone());
    }
}

fn collect_refs_stmt(
    stmt: &Stmt,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match stmt {
        Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => {
            collect_refs_rvalue(rvalue, enclosing, globals, out)
        }
        Stmt::Bind { name, value, .. } => {
            // A bare-assignment target is itself a reference (it may reassign an outer binding).
            out.insert(name.clone());
            atom_ref(value, out);
        }
        Stmt::Echo { value, .. } => atom_ref(value, out),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                atom_ref(value, out);
            }
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            atom_ref(cond, out);
            collect_refs_block(then_block, enclosing, globals, out);
            if let Some(else_block) = else_block {
                collect_refs_block(else_block, enclosing, globals, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_refs_block(cond, enclosing, globals, out);
            collect_refs_block(body, enclosing, globals, out);
        }
        Stmt::For { iterable, body, .. } => {
            atom_ref(iterable, out);
            collect_refs_block(body, enclosing, globals, out);
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            atom_ref(scrutinee, out);
            for arm in arms {
                // Names the arm pattern binds are local to the arm — collect the guard's and
                // body's refs, then remove them so they are not reported as free.
                let mut bound = HashSet::new();
                pattern_names(&arm.pattern, &mut bound);
                let mut arm_refs = BTreeSet::new();
                if let Some(guard) = &arm.guard {
                    collect_refs_block(&guard.block, enclosing, globals, &mut arm_refs);
                }
                collect_refs_block(&arm.body, enclosing, globals, &mut arm_refs);
                out.extend(arm_refs.into_iter().filter(|n| !bound.contains(n)));
            }
        }
        Stmt::Logical { left, right, .. } => {
            atom_ref(left, out);
            collect_refs_block(right, enclosing, globals, out);
        }
        Stmt::Coalesce {
            value, fallback, ..
        } => {
            atom_ref(value, out);
            collect_refs_block(fallback, enclosing, globals, out);
        }
        Stmt::Decl(Decl::Fn { func, .. }) => {
            // The nested `fn`'s free variables bubble up (minus its own params/locals).
            out.extend(free_vars(
                &func.params,
                &func.defaults,
                &func.body,
                enclosing,
                globals,
                func.captures.as_deref(),
            ));
        }
        Stmt::Decl(_)
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. } => {}
    }
}

fn collect_refs_rvalue(
    rvalue: &Rvalue,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match rvalue {
        Rvalue::Closure { func, .. } => {
            out.extend(free_vars(
                &func.params,
                &func.defaults,
                &func.body,
                enclosing,
                globals,
                func.captures.as_deref(),
            ));
        }
        _ => for_each_rvalue_atom(rvalue, &mut |atom| atom_ref(atom, out)),
    }
}

// --- Nested closures' free variables (the celling decision) ------------------------------------

/// Collect the free variables of the closures/`fn`s nested in `block` (not this block's own
/// ident references) — the names that, if local here, must be celled.
fn collect_nested_frees_block(
    block: &Block,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    for stmt in &block.stmts {
        collect_nested_frees_stmt(stmt, enclosing, globals, out);
    }
}

fn collect_nested_frees_stmt(
    stmt: &Stmt,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match stmt {
        Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => {
            collect_nested_frees_rvalue(rvalue, enclosing, globals, out)
        }
        Stmt::Decl(Decl::Fn { func, .. }) => {
            out.extend(free_vars(
                &func.params,
                &func.defaults,
                &func.body,
                enclosing,
                globals,
                func.captures.as_deref(),
            ));
        }
        Stmt::If {
            then_block,
            else_block,
            ..
        } => {
            collect_nested_frees_block(then_block, enclosing, globals, out);
            if let Some(else_block) = else_block {
                collect_nested_frees_block(else_block, enclosing, globals, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_nested_frees_block(cond, enclosing, globals, out);
            collect_nested_frees_block(body, enclosing, globals, out);
        }
        Stmt::For { body, .. } => collect_nested_frees_block(body, enclosing, globals, out),
        Stmt::Match { arms, .. } => {
            // An arm's pattern bindings are locals of *this* function (`collect_bindings_stmt`
            // records them), exactly like a `for` variable — so a closure in the arm that captures
            // one must make it celled. They are therefore NOT subtracted here: this collector feeds
            // the celling decision, and filtering them out left the binding uncelled while the
            // closure still captured it (the compiler then had no cell to source the capture from).
            // The mirror collector `collect_refs_stmt` *does* subtract them, because there the
            // question is which names escape to an *enclosing* function — and an arm binding never
            // does.
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_nested_frees_block(&guard.block, enclosing, globals, out);
                }
                collect_nested_frees_block(&arm.body, enclosing, globals, out);
            }
        }
        Stmt::Logical { right, .. } => collect_nested_frees_block(right, enclosing, globals, out),
        Stmt::Coalesce { fallback, .. } => {
            collect_nested_frees_block(fallback, enclosing, globals, out)
        }
        Stmt::Bind { .. }
        | Stmt::Echo { .. }
        | Stmt::Return { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. }
        | Stmt::Decl(_) => {}
    }
}

fn collect_nested_frees_rvalue(
    rvalue: &Rvalue,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    if let Rvalue::Closure { func, .. } = rvalue {
        out.extend(free_vars(
            &func.params,
            &func.defaults,
            &func.body,
            enclosing,
            globals,
            func.captures.as_deref(),
        ));
    }
    // No other rvalue nests a function body — atoms cannot contain closures.
}

// --- Atom traversal of an rvalue's operands ----------------------------------------------------

/// Visit each [`Atom`] operand of an rvalue (a [`Rvalue::Closure`]'s captured names are *not*
/// reached this way — they are handled separately by the free-variable bubble-up).
fn for_each_rvalue_atom(rvalue: &Rvalue, f: &mut impl FnMut(&Atom)) {
    match rvalue {
        Rvalue::Use(a) => f(a),
        Rvalue::Unary { operand, .. } => f(operand),
        Rvalue::MaskWidth { operand, .. } => f(operand),
        Rvalue::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::WideInt { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::WidthIntMethod { receiver, args, .. } => {
            f(receiver);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Call { callee, args, .. } => {
            f(callee);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Method { receiver, args, .. } | Rvalue::TraitMethod { receiver, args, .. } => {
            f(receiver);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Field { receiver, .. } => f(receiver),
        // A method handle carries only static strings — no operand atoms, no free variables.
        Rvalue::MethodHandle { .. } => {}
        // A bound handle captures its receiver operand.
        Rvalue::BoundHandle { recv, .. } => f(recv),
        Rvalue::SetField {
            receiver, value, ..
        } => {
            f(receiver);
            f(value);
        }
        Rvalue::Index {
            receiver, index, ..
        }
        | Rvalue::IndexField {
            receiver, index, ..
        } => {
            f(receiver);
            f(index);
        }
        Rvalue::List { items, .. } | Rvalue::Tuple { items, .. } => items.iter().for_each(&mut *f),
        Rvalue::PackedListNew { .. } => {}
        Rvalue::PackedListPush { list, value, .. } => {
            f(list);
            f(value);
        }
        Rvalue::TupleIndex { receiver, .. } => f(receiver),
        Rvalue::Map { entries, .. } => {
            for (k, v) in entries {
                f(k);
                f(v);
            }
        }
        Rvalue::Range { start, end, .. } => {
            f(start);
            f(end);
        }
        Rvalue::Object { fields, spread, .. } => {
            if let Some((a, _)) = spread {
                f(a);
            }
            for init in fields {
                f(&init.value);
            }
        }
        Rvalue::Interp { parts, .. } => {
            for part in parts {
                if let noeta_ir::InterpPart::Hole { atom, .. } = part {
                    f(atom);
                }
            }
        }
        Rvalue::Try { operand, .. }
        | Rvalue::As { operand, .. }
        | Rvalue::TypeTest { operand, .. }
        | Rvalue::TypeOf { operand, .. }
        | Rvalue::TypeArgName { operand, .. }
        | Rvalue::FieldsOf { operand, .. }
        | Rvalue::TraitsOf { operand, .. } => f(operand),
        Rvalue::ParamsOf { target, .. } | Rvalue::ReturnsOf { target, .. } => f(target),
        Rvalue::FieldSpecsOf { name, .. } => f(name),
        Rvalue::Construct { name, fields, .. } => {
            f(name);
            f(fields);
        }
        Rvalue::DecodeTyped { name, text, .. } => {
            f(name);
            f(text);
        }
        Rvalue::MakeGen { step, .. } => f(step),
        Rvalue::MakeFuture { thunk, .. } => f(thunk),
        Rvalue::RunFuture { future, .. } => f(future),
        Rvalue::PollFuture { future, .. } => f(future),
        Rvalue::Pending { .. } => {}
        Rvalue::Spawn { future, .. } => f(future),
        Rvalue::ScopeBegin { .. } => {}
        Rvalue::ScopeReady { scope, .. } | Rvalue::ScopeEndAt { scope, .. } => f(scope),
        Rvalue::SpawnIsolate { callee, args, .. } => {
            f(callee);
            args.iter().for_each(f);
        }
        Rvalue::MakeChannel { capacity, .. } => f(capacity),
        Rvalue::FromBytes { blob, .. } => f(blob),
        Rvalue::Invoke {
            recv, name, args, ..
        } => {
            if let Some(recv) = recv {
                f(recv);
            }
            f(name);
            f(args);
        }
        Rvalue::TypedModuleCall { args, dynamic, .. } => {
            args.iter().for_each(&mut *f);
            // The hidden type-argument slot (F2b) is a read — a closure inside a forwarding fn
            // captures it like any local.
            dynamic.iter().for_each(&mut *f);
        }
        Rvalue::TypedMethodCall {
            recv,
            args,
            dynamic,
            ..
        } => {
            f(recv);
            args.iter().for_each(&mut *f);
            dynamic.iter().for_each(&mut *f);
        }
        Rvalue::AttributesOf { dynamic, .. } => dynamic.iter().for_each(&mut *f),
        // No operands (or handled elsewhere).
        Rvalue::Closure { .. }
        | Rvalue::RolesOf { .. }
        | Rvalue::ModuleFn { .. }
        | Rvalue::NativeModule { .. } => {}
    }
}
