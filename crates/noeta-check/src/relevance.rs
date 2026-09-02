//! **Destructor-relevance analysis**: the destruct-reachability
//! fixpoint over the declared type graph, the per-type/per-binding relevance queries, and the
//! parameter-relevance recording walk. A pure analysis with its own output type
//! ([`DestructorRelevance`]) — moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;

/// Whether dropping a value of type `ty` could run *some* `destruct` block — destructor-relevance,
/// evaluated against the set of destruct-**reachable** type names. **Conservative in
/// the "assume relevant" direction**: `dyn`/`Unknown`/a kind-type/a function value (which may own
/// captures) and any named type or argument in `reachable` count as relevant; only the primitive
/// scalars and aggregates built purely from non-relevant parts are ruled out. So a `false` result
/// is a proof of non-relevance, while a `true` may be an over-approximation — exactly the direction
/// that keeps Phase 4's destructor firing sound.
pub(crate) fn type_relevant(ty: &Type, reachable: &HashSet<String>) -> bool {
    match ty {
        // No value, or a primitive scalar: a drop runs no destructor.
        Type::Unit
        | Type::Int
        | Type::Float
        | Type::F32
        | Type::F64
        | Type::IntN { .. }
        | Type::Bool
        | Type::String
        | Type::Bytes
        // Uninhabited: there is no value of this type to drop, so no destructor to run.
        | Type::Never => false,
        // Missing information / the dynamic top / an abstract kind / a function value: assume
        // relevant. So is a still-un-instantiated type PARAMETER, for the same reason and the same
        // direction: it could be instantiated with a destructor-bearing type. (Reached only where
        // the caller did not already substitute it to `dyn` — `params_to_dyn` normally has.)
        Type::Unknown
        | Type::Dyn
        | Type::DynTrait(_)
        | Type::Kind(_)
        | Type::Param(_)
        | Type::Fn { .. } => true,
        // Aggregates are relevant exactly when a part they own is.
        Type::List(e) | Type::Set(e) | Type::Option(e) => type_relevant(e, reachable),
        Type::Map(k, v) => type_relevant(k, reachable) || type_relevant(v, reachable),
        Type::Result(t, e) => type_relevant(t, reachable) || type_relevant(e, reachable),
        Type::Union(members) => members.iter().any(|m| type_relevant(m, reachable)),
        // A tuple is relevant exactly when one of its elements is (like a list).
        Type::Tuple(elements) => elements.iter().any(|e| type_relevant(e, reachable)),
        // A `Future`/`Iterator` (an async future, a generator, or a lazy iterator) captures the locals
        // of the expression that built it in an opaque step closure — like a `Fn` value, its captures
        // are invisible in its type arguments, so a `Future<int>` may still hold a destructor-bearing
        // captured local. Conservatively relevant, so its drop is destructor-aware (matching `Fn`).
        Type::Named(name, _) if name == "Future" || name == "Iterator" => true,
        // A declared type: relevant if it (transitively) reaches a destructor, or any type argument
        // does (covers generic containers like `Box<Resource>`).
        Type::Named(name, args) => {
            reachable.contains(name) || args.iter().any(|a| type_relevant(a, reachable))
        }
    }
}
impl Checker {
    /// Compute destruct-reachability, after [`Self::collect`] has registered every
    /// type. A type name is reachable when dropping a value of that type could run *some* `destruct`
    /// block: it has its own (a class in `destructor_classes`), or one of its fields / variant
    /// payloads / collection elements does — a monotone fixpoint over the declared type graph. Then
    /// records the parameters whose type is reachable (locals are recorded inline during checking).
    pub(crate) fn compute_relevance(&mut self, program: &Program) {
        let mut reachable = self.symbols.destructor_classes.clone();
        // Only a *generic* type's fields need the parameter→`dyn` substitution below (which
        // materializes a fresh substituted tree); the common non-generic field is queried in
        // place, clone-free — this fixpoint re-walks every declared field per iteration per
        // check, so the per-field clone was a standing allocation tax (audit-3 Finding 12).
        let relevant = |ty: &Type, params: &ParamSet, reachable: &HashSet<String>| {
            if params.is_empty() {
                type_relevant(ty, reachable)
            } else {
                type_relevant(&params_to_dyn(ty, params), reachable)
            }
        };
        loop {
            let mut changed = false;
            // A field/payload mentioning a **generic parameter** is conservatively relevant: the
            // parameter could be instantiated with a destructor-bearing type, and the runtime erases
            // the argument (the backends gate the container-first destructor walk on the value's shape
            // *name* alone), so a generic container's name must be marked destruct-reachable whenever a
            // payload mentions a parameter. Substituting each parameter to `dyn` (which is relevant)
            // before the check achieves exactly that; a concrete field is unaffected.
            for (name, fields) in &self.symbols.records {
                let params: ParamSet = self
                    .symbols
                    .generic_types
                    .get(name)
                    .map(|ps| ps.iter().map(|p| p.id).collect())
                    .unwrap_or_default();
                if !reachable.contains(name)
                    && fields
                        .iter()
                        .any(|(_, ty)| relevant(ty, &params, &reachable))
                {
                    reachable.insert(name.clone());
                    changed = true;
                }
            }
            for (name, variants) in &self.symbols.enums {
                let params: ParamSet = self
                    .symbols
                    .generic_types
                    .get(name)
                    .map(|ps| ps.iter().map(|p| p.id).collect())
                    .unwrap_or_default();
                if !reachable.contains(name)
                    && variants
                        .iter()
                        .any(|v| v.fields.iter().any(|ty| relevant(ty, &params, &reachable)))
                {
                    reachable.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Export the per-type reachable set for the backends' field-walk gate, alongside
        // the per-binding sets the drop pass reads. **Ordered** ([`DestructorRelevance::reachable_types`]):
        // the compiler collects it straight into the serialized `Module::destruct_reachable`, so a
        // hash-derived order would be a per-process-random difference in the compiled bytes. The
        // fixpoint above stays hash-backed — it is all membership queries — and so does the copy the
        // analysis keeps for [`Self::type_relevant`]; this costs exactly the one full clone it always
        // did, now landing in a set that has an order.
        self.relevance.reachable_types = reachable.iter().cloned().collect();
        self.symbols.destruct_reachable = reachable;
        self.record_param_relevance(program);
    }

    /// Whether a binding of type `ty` is destructor-relevant under the computed reachable set.
    pub(crate) fn type_relevant(&self, ty: &Type) -> bool {
        type_relevant(ty, &self.symbols.destruct_reachable)
    }

    /// Record each `fn`/method parameter whose declared type is destruct-reachable, keyed by
    /// `(function span, parameter name)` — matching how the Core IR identifies a parameter (its
    /// `Func.span` + the bare name). Parameter types come from the annotation (`param_type`), not
    /// inference, so this is a standalone statement walk. Closure parameters (an `Expr::Closure`,
    /// not a statement) are not recorded here, so they default to conservatively-relevant in the
    /// drop pass — sound, and closure-parameter precision is marginal.
    pub(crate) fn record_param_relevance(&mut self, program: &Program) {
        for stmt in &program.stmts {
            self.record_param_relevance_stmt(stmt);
        }
    }

    pub(crate) fn record_param_relevance_fn(
        &mut self,
        fn_span: Span,
        params: &[Param],
        body: &[Stmt],
    ) {
        for p in params {
            if self.type_relevant(&self.annot_param(p)) {
                self.relevance.params.insert((fn_span, p.name.clone()));
            }
        }
        for stmt in body {
            self.record_param_relevance_stmt(stmt);
        }
    }

    pub(crate) fn record_param_relevance_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Fn(decl) => self.record_param_relevance_fn(decl.span, &decl.params, &decl.body),
            Stmt::Class(c) => {
                for m in c
                    .methods
                    .iter()
                    .chain(c.impls.iter().flat_map(|b| b.methods.iter()))
                {
                    self.record_param_relevance_fn(m.span, &m.params, &m.body);
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                then_body
                    .iter()
                    .for_each(|s| self.record_param_relevance_stmt(s));
                if let Some(b) = else_body {
                    b.iter().for_each(|s| self.record_param_relevance_stmt(s));
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                body.iter()
                    .for_each(|s| self.record_param_relevance_stmt(s));
            }
            _ => {}
        }
    }
}
