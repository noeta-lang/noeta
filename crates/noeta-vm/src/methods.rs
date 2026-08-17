//! Builtin / receiver **method dispatch**: the `call_*_method` cluster and the
//! small per-receiver helpers (list / set / map / vec / iter / file-handle
//! methods, the in-place mutation paths, and the stdlib arity / coercion
//! helpers). Every item here is an `impl Vm` method moved verbatim out of the
//! crate root; the `dispatch` loop in `lib.rs` is the sole caller. Kept in its
//! own file purely to shrink `lib.rs` — no behavior change.

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_value::Value;

use crate::*;

/// The per-call-site routing decision for a method on an extern receiver (H5 perf), cached in
/// `Op::CallMethod`'s route cache keyed by the pointer of the extern value's interned
/// **qualified identity** ([`noeta_stdlib::ExternValue::type_identity`]). The routes carry no
/// name of their own: the dispatch site already holds the identity it cached under, which is the
/// exact string ctx dispatch and the read gates key on.
#[derive(Clone, Copy)]
pub(crate) enum ExternRoute {
    /// A declared arena read ([`noeta_stdlib::ExtType`]`::arena_getter`): inline to an arena
    /// load while the type's gate is open; the full ctx dispatch while closed.
    FastRead {
        project: fn(&dyn noeta_stdlib::ExternValue) -> u32,
    },
    /// A ctx-table method — straight to the type's ctx dispatch.
    Ctx,
    /// The plain by-value dispatch (including unknown methods — the shared error path).
    Plain,
}

/// Resolve the route for `method` on the extern type with qualified identity `identity` — the
/// uncached registry walk a route-cache miss performs. The caller passes its VM's registry
/// (instance-registry IR3); the walk is `#[cold]` (only a route-cache miss reaches it), so the
/// extra argument never touches the hot per-op path.
#[cold]
pub(crate) fn resolve_extern_route(
    reg: &noeta_stdlib::registry::Registry,
    identity: &str,
    method: &str,
) -> ExternRoute {
    let Some(ext) = reg.find_type_qualified(identity) else {
        return ExternRoute::Plain;
    };
    if let Some((getter, project)) = ext.arena_getter
        && getter == method
    {
        return ExternRoute::FastRead { project };
    }
    if ext.ctx_methods.iter().any(|m| m.name == method) {
        return ExternRoute::Ctx;
    }
    ExternRoute::Plain
}

impl<'m> Vm<'m> {
    /// The **ordering hint** the checker recorded at `span` — a `.sorted()`/`.min()`/`.max()`/
    /// `.keys()`/`.values()` call, or a `for` loop, over a value whose static type carries an
    /// unsigned 64-bit integer. `None` for every unrecorded site, which is nearly all of them, so
    /// the ordinary path costs one length test — the `is_empty` short-circuit is what keeps this off
    /// the loop-entry `IterSnapshot`'s cost, which would otherwise hash a span per loop entry. The
    /// tree-walker reads the same hint off its IR node instead of a side table; both walk it
    /// identically.
    pub(crate) fn order_hint(&self, span: &Span) -> Option<&noeta_ast::RenderHint> {
        if self.order_hints.is_empty() {
            return None;
        }
        self.order_hints.get(span)
    }

    /// The push hint recorded at `span` — the deferred twin of [`Vm::order_hint`], read once when a
    /// native call that BINDS a value for later serialization builds its ctx. Same `is_empty`
    /// short-circuit, for the same reason: nearly every program has no entry at all.
    pub(crate) fn binding_hint(&self, span: &Span) -> Option<&noeta_ast::RenderHint> {
        if self.binding_hints.is_empty() {
            return None;
        }
        self.binding_hints.get(span)
    }

    /// A Ring 1 list method (`reverse`/`contains`/`join`). Mirrors the tree-walker's
    /// `call_list_method`; the result is a freshly-owned value (refcount 1). The receiver's
    /// elements shared from `list_items` are not retained, so any element placed into a *new*
    /// list must be retained first (the list then owns that reference).
    /// Dispatch a Ring 1 list method. A packed list (P-PACK 2.4) has no specialized list methods
    /// yet, so it is materialized to a temporary boxed list, dispatched, and released — the result
    /// is observably identical to the boxed equivalent. A boxed list dispatches directly.
    pub(crate) fn call_list_method(
        &mut self,
        list: Value,
        method: noeta_stdlib::ListMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if list.is_packed_list() {
            // Selection producers keep the list *flat* — `reverse`/`slice` build a new packed buffer
            // by copying the selected elements' word-blocks (P-PACK 2.6), instead of demoting to N
            // boxed objects. Their arity/bounds checks and errors mirror the boxed arms exactly, so
            // the result is observably identical. Every other method demotes (materialize-on-read).
            match method {
                noeta_stdlib::ListMethod::Reverse => {
                    self.stdlib_arity(name, args, 0, span)?;
                    let n = list.list_len().expect("packed list");
                    let indices: Vec<usize> = (0..n).rev().collect();
                    return Ok(list.packed_select(&indices));
                }
                noeta_stdlib::ListMethod::Slice => {
                    self.stdlib_arity_range(name, args, 1, 2, span)?;
                    let start = self.stdlib_int(name, args[0], span)?;
                    let len = list.list_len().expect("packed list");
                    let end = self.stdlib_opt_int(name, args, 1, len as i64, span)?;
                    if start < 0 || end < start || end as usize > len {
                        let error = noeta_stdlib::slice_bounds_error(start, end, len);
                        return Err(self.std_dispatch_error(error, span));
                    }
                    let indices: Vec<usize> = (start as usize..end as usize).collect();
                    return Ok(list.packed_select(&indices));
                }
                noeta_stdlib::ListMethod::Set => {
                    self.stdlib_arity(name, args, 2, span)?;
                    let i = self.stdlib_int(name, args[0], span)?;
                    let len = list.list_len().expect("packed list");
                    if i < 0 || i as usize >= len {
                        return Err(self.error(
                            DiagnosticCode::IndexOutOfBounds,
                            span,
                            format!("index {i} out of bounds for list of length {len}"),
                        ));
                    }
                    // Stays flat unless the new element does not pack (impossible for a well-typed
                    // `List<packed>.set` — then demote).
                    if let Some(result) = list.packed_set(i as usize, args[1]) {
                        return Ok(result);
                    }
                }
                _ => {}
            }
            let boxed = list.realize_list();
            let result = self.call_list_method_boxed(boxed, method, name, args, span);
            boxed.release();
            result
        } else {
            self.call_list_method_boxed(list, method, name, args, span)
        }
    }

    /// As [`Self::call_list_method`], but `list` is guaranteed to be a boxed list (the caller has
    /// materialized any packed receiver).
    fn call_list_method_boxed(
        &mut self,
        list: Value,
        method: noeta_stdlib::ListMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = list.list_items().expect("list receiver");
        match method {
            noeta_stdlib::ListMethod::Reverse => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut reversed = items;
                reversed.reverse();
                for &element in &reversed {
                    retain(element);
                }
                Ok(Value::list(reversed))
            }
            noeta_stdlib::ListMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            noeta_stdlib::ListMethod::Join => {
                self.stdlib_arity_range(name, args, 0, 1, span)?;
                let separator = match args.first() {
                    Some(&arg) => self.stdlib_string(name, arg, span)?,
                    None => String::new(),
                };
                let joined = items
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(&separator);
                Ok(Value::string(&joined))
            }
            noeta_stdlib::ListMethod::Sorted => {
                self.stdlib_arity(name, args, 0, span)?;
                // Mutual orderability check against the first element (homogeneous numbers or
                // strings — or derived-`Comparable` structs/enums, which order structurally via
                // `compare_values`); a stable sort then matches the tree-walker element-for-element.
                if items
                    .iter()
                    .any(|&item| noeta_value::compare_values(items[0], item).is_none())
                {
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.std_dispatch_error(error, span));
                }
                // A `List<u64>` (or a list carrying one in a payload the runtime cannot describe)
                // orders under the checker's hint for this call, so the erased words read unsigned.
                let elem = self.order_hint(&span).and_then(|h| h.elements());
                let mut sorted = items;
                sorted.sort_by(|&a, &b| {
                    noeta_value::compare_values_hinted(a, b, elem)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                for &element in &sorted {
                    retain(element);
                }
                Ok(Value::list(sorted))
            }
            noeta_stdlib::ListMethod::Slice => {
                self.stdlib_arity_range(name, args, 1, 2, span)?;
                let start = self.stdlib_int(name, args[0], span)?;
                let len = items.len();
                let end = self.stdlib_opt_int(name, args, 1, len as i64, span)?;
                if start < 0 || end < start || end as usize > len {
                    let error = noeta_stdlib::slice_bounds_error(start, end, len);
                    return Err(self.std_dispatch_error(error, span));
                }
                let slice: Vec<Value> = items[start as usize..end as usize].to_vec();
                for &element in &slice {
                    retain(element);
                }
                Ok(Value::list(slice))
            }
            noeta_stdlib::ListMethod::First => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.first() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            noeta_stdlib::ListMethod::Last => {
                self.stdlib_arity(name, args, 0, span)?;
                Ok(match items.last() {
                    Some(&value) => {
                        retain(value);
                        make_some(value)
                    }
                    None => make_none(),
                })
            }
            noeta_stdlib::ListMethod::ToSet => {
                self.stdlib_arity(name, args, 0, span)?;
                match canonical_set(&items) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        let set = Value::set(canonical);
                        // Carry the element type from the source list's `List<T>` tag onto the
                        // resulting `Set<T>` (R1 set tags) — sets have no literal, so `to_set` is the
                        // one construction point where the element type is known.
                        set.set_reflect(set_tag_from_list(list));
                        Ok(set)
                    }
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.std_dispatch_error(error, span))
                    }
                }
            }
            noeta_stdlib::ListMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let i = self.stdlib_int(name, args[0], span)?;
                if i < 0 || i as usize >= items.len() {
                    return Err(self.error(
                        DiagnosticCode::IndexOutOfBounds,
                        span,
                        format!("index {i} out of bounds for list of length {}", items.len()),
                    ));
                }
                // Replace the slot; the displaced old element is just dropped from the clone (it was
                // never retained by `list_items`). Every element the new list ends up holding is
                // retained once (the new list is a fresh owner).
                let mut new = items;
                new[i as usize] = args[1];
                for &element in &new {
                    retain(element);
                }
                Ok(Value::list(new))
            }
        }
    }

    /// A Ring 1 set method (`contains`/`union`/`intersection`). Mirrors the tree-walker's
    /// `call_set_method`. The receiver's elements (from `set_items`) are already canonical and
    /// shared (not retained); any element placed into a new set is retained first.
    pub(crate) fn call_set_method(
        &mut self,
        set: Value,
        method: noeta_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let items = set.set_items().expect("set receiver");
        match method {
            noeta_stdlib::SetMethod::Contains => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let found = items.iter().any(|&item| {
                    apply_binary(BinaryOp::Eq, item, target)
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                });
                Ok(Value::bool(found))
            }
            noeta_stdlib::SetMethod::Union => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                let mut combined = items;
                combined.extend(other);
                // Both operands are valid sets, so every element is orderable.
                let canonical = canonical_set(&combined).expect("set elements are orderable");
                for &element in &canonical {
                    retain(element);
                }
                Ok(Value::set(canonical))
            }
            noeta_stdlib::SetMethod::Intersection => {
                self.stdlib_arity(name, args, 1, span)?;
                let other = self.stdlib_set(name, args[0], span)?;
                // `items` is already canonical, so filtering preserves sorted, de-duplicated order.
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        other.iter().any(|&o| {
                            apply_binary(BinaryOp::Eq, item, o)
                                .ok()
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                        })
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
            noeta_stdlib::SetMethod::Add => {
                self.stdlib_arity(name, args, 1, span)?;
                let mut combined = items;
                combined.push(args[0]);
                match canonical_set(&combined) {
                    Some(canonical) => {
                        for &element in &canonical {
                            retain(element);
                        }
                        Ok(Value::set(canonical))
                    }
                    None => {
                        let error = noeta_stdlib::unorderable_error(name);
                        Err(self.std_dispatch_error(error, span))
                    }
                }
            }
            noeta_stdlib::SetMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let target = args[0];
                let kept: Vec<Value> = items
                    .into_iter()
                    .filter(|&item| {
                        !apply_binary(BinaryOp::Eq, item, target)
                            .ok()
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .collect();
                for &element in &kept {
                    retain(element);
                }
                Ok(Value::set(kept))
            }
        }
    }

    /// Read a set argument for a set method, raising the shared `noeta-stdlib` type error. Returns
    /// the set's canonical elements (shared, not retained).
    fn stdlib_set(&mut self, name: &str, value: Value, span: Span) -> Result<Vec<Value>, Abort> {
        match value.set_items() {
            Some(items) => Ok(items),
            None => {
                let error = noeta_stdlib::type_error(name, "set");
                Err(self.std_dispatch_error(error, span))
            }
        }
    }

    /// Dispatch a Ring 2 native module function call (`json.parse(...)`). Mirrors the
    /// tree-walker's `call_native_module`.
    pub(crate) fn call_native_module(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // (The virtual-module intercept died with higher-order-abi H5: `task` migrated at H0/H2,
        // `http.serve` at H3, and `reactive` — the last virtual module — at H5. Every std module
        // now dispatches through the registry arms below.)
        // A function registered in the native-extension registry dispatches through the shared
        // seam: project arguments onto `NativeValue`, run the one shared dispatch body (host
        // threaded in), and materialize the `NativeOut` result (the result shape supplied from the
        // function's `RetTy`). Routing is per-function so a partially-migrated module (`vec`/`quat`,
        // whose bulk `*_all` kernels stay per-backend) falls through for its unmigrated functions.
        // Bound once (instance-registry IR3): `reg` is `&'static`, so it outlives the `&mut self`
        // borrows below (the host, the executor) and every native lookup routes through this VM's
        // registry rather than the process-global default.
        let reg = self.reg();
        if let Some(sig) = reg.find_function(module, func) {
            // A reflective module (`json`) marshals its arguments deeply (the recursive value tree
            // `json.stringify` introspects); every other module uses the cheap shallow projection.
            let deep = reg.find_module(module).is_some_and(|m| m.deep_marshal);
            let nargs: Vec<noeta_stdlib::NativeValue> = if deep {
                args.iter().map(|a| a.to_native_deep()).collect()
            } else {
                args.iter().map(|a| marshal_native_arg(*a, reg)).collect()
            };
            return match reg.dispatch(module, func, &mut *self.persist.host, &nargs) {
                // Async WORK (extern-types X5): ticket the descriptor on the executor and hand
                // back a leaf async-IO future `.await` later resolves — the same shape the old
                // per-backend `fs.*_async` intercept produced, now reached by ordinary dispatch.
                Ok(noeta_stdlib::NativeOut::Spawn(spawn)) => {
                    let id = self
                        .persist
                        .executor
                        .spawn_ext(&mut *self.persist.host, spawn.0);
                    Ok(Value::make_async_io(id))
                }
                Ok(out) => Ok(materialize_ext(out, sig.ret, args)),
                Err(error) => Err(self.std_dispatch_error(error, span)),
            };
        }
        // A registered **higher-order** function (higher-order-abi H0) dispatches through the
        // `NativeCtx` seam: opaque slots + backend re-entry instead of marshalled values. Checked
        // after the plain table — plain functions vastly outnumber ctx ones, and the two name
        // sets are disjoint, so order is behavior-neutral and keeps the common path lean.
        // (The last per-backend intercept — `vec`'s bulk `*_all` kernels — died with the N3.4
        // raw-buffer seam: they are ordinary ctx functions now, reached right here.)
        if reg.find_ctx_function(module, func).is_some() {
            return self.call_ctx_function(module, func, args, span);
        }
        let error = noeta_stdlib::no_function_error(module, func);
        Err(self.std_dispatch_error(error, span))
    }

    /// Dispatch a method on an extern-type receiver (extern-types X1) through its registered
    /// [`noeta_stdlib::ExtType`]'s shared dispatch — project the arguments, run the one shared
    /// body (host threaded in, receiver `&mut`), materialize the result. Mirrors the
    /// tree-walker's `call_extern_method`, so the two backends agree by construction.
    ///
    /// (The `Op::CallMethod` handler short-circuits extern receivers through the per-site route
    /// cache before reaching this chain — H5 perf; this entry stays for the non-op paths and
    /// resolves the same [`ExternRoute`] decisions uncached.)
    pub(crate) fn call_extern_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // ONE heap access + ONE registry lookup resolves everything the routing below needs
        // (H5 perf): the type entry, and — for a declared **gated arena read**
        // (`ExtType::arena_getter`) — the projected retained id. `reg` is bound once (`&'static`,
        // Copy) so the closures below capture it without holding a borrow of `self` (IR3).
        let reg = self.reg();
        let (identity, ext, fast_read) = recv.with_extern(|e| {
            let identity = e.type_identity();
            let ext = reg.find_type_qualified(identity);
            let fast = ext.and_then(|t| {
                let (getter, project) = t.arena_getter?;
                (getter == method).then(|| project(e))
            });
            (identity, ext, fast)
        });
        // The fast read: while the type's read gate is open — the overwhelmingly common state —
        // the whole call is an arena load + retain, no ctx machinery, which is what keeps a
        // `get()` hot loop at intercept speed. The gate set is keyed by the same qualified
        // identity the value carries.
        if let Some(retained) = fast_read
            && args.is_empty()
            && (self.persist.ext_closed_gates.is_empty()
                || !self.persist.ext_closed_gates.contains(&identity))
        {
            let value = self.persist.ext_arena[retained as usize].expect("a live arena entry");
            retain(value);
            return Ok(value);
        }
        // A type's **higher-order** methods (higher-order-abi H4) route through the ctx seam —
        // they call closures back and reach the retained arena, which the plain by-value
        // dispatch below cannot. Name sets are disjoint, so routing is per-method.
        if let Some(ext) = ext
            && ext.ctx_methods.iter().any(|m| m.name == method)
        {
            return self.call_ctx_type_method(identity, recv, method, args, span);
        }
        // A type declaring `deep_marshal` (the metrics instruments' `*_with(_, attrs)`) projects a
        // container argument to a full `NativeValue` tree; every other type uses the cheap shallow
        // projection (containers → `Opaque`).
        let deep = ext.is_some_and(|t| t.deep_marshal);
        // `&'static`, Copy — bound before the host borrow so the struct-arg marshal can consult the
        // registry (native value-struct → `Instance`).
        let reg = self.reg();
        let nargs: Vec<noeta_stdlib::NativeValue> = args
            .iter()
            .map(|a| {
                if deep {
                    a.to_native_deep()
                } else {
                    marshal_native_arg(*a, reg)
                }
            })
            .collect();
        let host = &mut *self.persist.host;
        let result = recv.with_extern_mut(|e| reg.dispatch_method(e, method, host, &nargs));
        match result {
            // Async WORK from an extern-type method (`Process.wait_async`, process-signals arc):
            // ticket the descriptor and hand back the async-IO future — mirrors the module-function
            // path above and the tree-walker's `call_extern_method`.
            Ok(noeta_stdlib::NativeOut::Spawn(spawn)) => {
                let id = self
                    .persist
                    .executor
                    .spawn_ext(&mut *self.persist.host, spawn.0);
                Ok(Value::make_async_io(id))
            }
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.std_dispatch_error(error, span)),
        }
    }

    /// Dispatch a **native class**'s instance method (native-extensibility S3 / Pass 2a) — the
    /// [`noeta_stdlib::ExtClass`] analogue of [`Self::call_extern_method`]. The receiver is a
    /// class-kind object; it crosses to the native `dispatch` as the whole instance marshalled to a
    /// [`noeta_stdlib::NativeValue::Instance`] (its fields by name), the same shape a class value
    /// takes arg-IN, so the method reads a field off it. Host threaded in, result materialized —
    /// mirrors the tree-walker's `call_native_class_method`, so the two backends agree.
    pub(crate) fn call_native_class_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // Bound once (`&'static`, Copy), so it survives the `&mut self` host borrow below (IR3).
        let reg = self.reg();
        // Resolve over BOTH classes and structs (fielded unification) — a value-struct method
        // dispatches through the same seam; only in-place mutation (`InstanceUpdate`) is class-only.
        let class = recv.shape().and_then(|s| reg.resolve_fielded(&s.name));
        let Some(class) = class else {
            return Err(self.error(
                DiagnosticCode::UnknownName,
                span,
                format!("no native fielded-type method `{method}`"),
            ));
        };
        let recv_native = marshal_native_arg(recv, reg);
        let nargs: Vec<noeta_stdlib::NativeValue> =
            args.iter().map(|a| marshal_native_arg(*a, reg)).collect();
        let host = &mut *self.persist.host;
        match (class.dispatch)(&recv_native, method, host, &nargs) {
            // A **struct** (value type) has no in-place mutation: reject an `InstanceUpdate` from a
            // struct dispatch as a runtime error rather than silently mutating a value. Mirrors the
            // tree-walker's guard, so both backends agree.
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { .. })
                if class.kind == noeta_stdlib::FieldedKind::Struct =>
            {
                Err(self.error(
                    DiagnosticCode::ImmutableField,
                    span,
                    format!(
                        "native struct method `{method}` returned an in-place mutation, but a \
                         struct `{}` is a value type — return a new value instead",
                        class.name
                    ),
                ))
            }
            // Boundary 1: an in-place instance mutation (class only). Apply each write to the LIVE
            // receiver's slot in place (`replace_slot` retains the new occupant and returns the
            // displaced one, whose destructor fires now), so every alias sees the change and identity
            // is preserved. Mirrors the tree-walker's `call_native_class_method`.
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { writes, ret }) => {
                for (field, value) in writes {
                    // A write must target a declared `mut` field — the ABI mirrors the source-level
                    // E0022-family rule; an unknown or non-`mut` field is a runtime error.
                    let slot = match class.fields.iter().find(|f| f.name == field) {
                        Some(spec) if spec.is_mut => recv.shape().and_then(|s| s.slot_of(&field)),
                        Some(_) => {
                            return Err(self.error(
                                DiagnosticCode::ImmutableField,
                                span,
                                format!(
                                    "native method `{method}` cannot write immutable field \
                                     `{field}` of class `{}`",
                                    class.name
                                ),
                            ));
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                span,
                                format!(
                                    "native method `{method}` writes unknown field `{field}` of \
                                     class `{}`",
                                    class.name
                                ),
                            ));
                        }
                    };
                    // The field is declared, so its slot resolves; defensively skip if not.
                    let Some(slot) = slot else { continue };
                    // Materialize the new occupant (refcount 1, mine), install it in place (the slot
                    // takes its own reference), then release my temporary so only the slot owns it.
                    let fresh = materialize_native(value);
                    let old = recv.replace_slot(slot, fresh);
                    self.release_value(fresh);
                    self.release_value(old);
                }
                Ok(materialize_native(*ret))
            }
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.std_dispatch_error(error, span)),
        }
    }

    /// Dispatch a **native enum**'s instance method (native-extensibility S1 / Slice B) — the
    /// [`noeta_stdlib::ExtEnum`] analogue of [`Self::call_native_class_method`], reusing the shared
    /// [`noeta_stdlib::NativeMethodDispatch`] seam. The receiver is an enum value; it crosses to the
    /// native `dispatch` as a [`noeta_stdlib::NativeValue::Variant`] (its case + declaration index +
    /// positional payload), the same shape an enum value takes arg-IN, so the method reads its
    /// payload off it. Host threaded in, result materialized — mirrors the tree-walker's
    /// `call_native_enum_method`, so the two backends agree. An enum is an **immutable value type**:
    /// a dispatch returning [`noeta_stdlib::NativeOut::InstanceUpdate`] is a runtime error, exactly
    /// as it is for a value struct.
    pub(crate) fn call_native_enum_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // Bound once (`&'static`, Copy), so it survives the `&mut self` host borrow below (IR3).
        let reg = self.reg();
        let en = recv.shape().and_then(|s| reg.resolve_enum(&s.name));
        let Some(en) = en else {
            return Err(self.error(
                DiagnosticCode::UnknownName,
                span,
                format!("no native enum method `{method}`"),
            ));
        };
        let recv_native = marshal_native_arg(recv, reg);
        let nargs: Vec<noeta_stdlib::NativeValue> =
            args.iter().map(|a| marshal_native_arg(*a, reg)).collect();
        let host = &mut *self.persist.host;
        match (en.dispatch)(&recv_native, method, host, &nargs) {
            // An enum is a value type — it has no in-place mutation. Reject an `InstanceUpdate` from
            // an enum dispatch as a runtime error rather than silently mutating a value, mirroring
            // the struct guard in `call_native_class_method` (and the tree-walker's enum guard).
            Ok(noeta_stdlib::NativeOut::InstanceUpdate { .. }) => Err(self.error(
                DiagnosticCode::ImmutableField,
                span,
                format!(
                    "native enum method `{method}` returned an in-place mutation, but an enum `{}` \
                     is an immutable value type — return a new value instead",
                    en.name
                ),
            )),
            Ok(out) => Ok(materialize_native(out)),
            Err(error) => Err(self.std_dispatch_error(error, span)),
        }
    }

    /// Dispatch an iterator method (Track I). Mirrors the tree-walker's `call_iter_method`. `next`/
    /// `collect`/`count` consume the cursor; `take`/`drop`/`chain` build a new adapter that retains
    /// the receiver (and `chain`'s argument) — the same retain pattern as `iter()`, leak-verified.
    pub(crate) fn call_iter_method(
        &mut self,
        recv: Value,
        method: noeta_stdlib::IterMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        use noeta_stdlib::IterMethod as M;
        Ok(match method {
            M::Next => {
                self.stdlib_arity(name, args, 0, span)?;
                let stepped = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    recv.iter_next_apply(&mut apply)
                };
                match stepped {
                    Ok(Some(element)) => make_some(element),
                    Ok(None) => make_none(),
                    Err(err) => return Err(self.iter_abort(err, span)),
                }
            }
            M::Collect => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut out = Vec::new();
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            Ok(Some(e)) => out.push(e),
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    for v in out {
                        release(v); // free the elements collected before the closure aborted
                    }
                    return Err(self.iter_abort(err, span));
                }
                Value::list(out)
            }
            M::Count => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut n = 0i64;
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            // Drain the iterator, releasing each element it retained.
                            Ok(Some(e)) => {
                                e.release();
                                n += 1;
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    return Err(self.iter_abort(err, span));
                }
                Value::int(n)
            }
            M::Take | M::Drop => {
                self.stdlib_arity(name, args, 1, span)?;
                let n = self.stdlib_int(name, args[0], span)?.max(0) as usize;
                if method == M::Take {
                    Value::iter_take(recv, n)
                } else {
                    Value::iter_drop(recv, n)
                }
            }
            M::Chain => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_chain(recv, args[0])
            }
            M::Enumerate => {
                self.stdlib_arity(name, args, 0, span)?;
                Value::iter_enumerate(recv)
            }
            M::Zip => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_zip(recv, args[0])
            }
            M::Map => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_map(recv, args[0])
            }
            M::Filter => {
                self.stdlib_arity(name, args, 1, span)?;
                Value::iter_filter(recv, args[0])
            }
            M::Sum => {
                self.stdlib_arity(name, args, 0, span)?;
                // A directly list-backed iterator (`xs.iter().sum()`, the canonical form) delegates to
                // the eager list reduction over its remaining elements — so a packed narrow-width list
                // folds its buffer and width-wraps *identically* to `xs.sum()` (no divergence). An
                // adapter chain (`take`/`map`/…) falls through to the generic fold, where the element
                // type is already a 64-bit `int`/`float`, so no width-wrapping is at stake.
                if let Some((list, from)) = recv.iter_drain_list() {
                    return self.call_list_reduction(list, "sum", from, span);
                }
                // A narrow-width source (`xs.iter().take(k)` over a `List<i32>`, …): the generic fold
                // accumulates at 64 bits, so mask the integer total back to the element width at the
                // end — the same wrap `xs.sum()` applies — so a narrow-typed iterator reduction agrees
                // (array-ops arc). Traced through the width-preserving adapters only.
                let narrow = recv.iter_narrow_width();
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                let mut bad: Option<&'static str> = None;
                let result = {
                    let mut apply =
                        |func: Value, arg: Value| self.call_value(func, vec![arg], span);
                    loop {
                        match recv.iter_next_apply(&mut apply) {
                            Ok(Some(e)) => {
                                if let Some(i) = e.as_int() {
                                    int_total = int_total.wrapping_add(i);
                                } else if let Some(f) = e.as_float() {
                                    any_float = true;
                                    float_total += f;
                                } else {
                                    bad = Some(e.type_name());
                                    e.release();
                                    break Ok(());
                                }
                                e.release();
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        }
                    }
                };
                if let Err(err) = result {
                    return Err(self.iter_abort(err, span));
                }
                if let Some(found) = bad {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects numeric elements, found {found}"),
                    ));
                }
                if any_float {
                    Value::float(float_total + int_total as f64)
                } else if let Some((signed, bits)) = narrow {
                    Value::int(noeta_stdlib::mask_to_width(int_total, signed, bits))
                } else {
                    Value::int(int_total)
                }
            }
        })
    }

    /// Advance an iterator one element for a streaming `for` (Track I.2) — drives `iter_next_apply`
    /// with the closure applier (so `map`/`filter` run) and maps an abort to the VM's native error.
    pub(crate) fn iter_for_next(
        &mut self,
        iter: Value,
        span: Span,
    ) -> Result<Option<Value>, Abort> {
        let stepped = {
            let mut apply = |func: Value, arg: Value| self.call_value(func, vec![arg], span);
            iter.iter_next_apply(&mut apply)
        };
        stepped.map_err(|err| self.iter_abort(err, span))
    }

    /// Map an iterator-pull abort (Track I.1c) back into the VM's native error: a closure failure
    /// carries its `Abort` through unchanged; a non-bool `filter` verdict becomes a `TypeMismatch`.
    fn iter_abort(&mut self, err: noeta_value::IterAbort<Abort>, span: Span) -> Abort {
        match err {
            noeta_value::IterAbort::Closure(abort) => abort,
            noeta_value::IterAbort::FilterNotBool(found) => self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`filter` predicate must return a bool, found {found}"),
            ),
        }
    }

    /// A Ring 1 map method (`keys`/`values`/`has`). Mirrors the tree-walker's `call_map_method`.
    pub(crate) fn call_map_method(
        &mut self,
        map: Value,
        method: noeta_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match method {
            noeta_stdlib::MapMethod::Keys => {
                self.stdlib_arity(name, args, 0, span)?;
                let mut keys = map.map_keys().expect("map receiver");
                // A `Map<u64, _>`'s keys are handed back in the order the *type* states, not the
                // erased word's. The map itself is untouched: its key placement is an identity
                // order (see `noeta_ast::render_hint`), and this only re-sorts the answer.
                if let Some(key) = self.order_hint(&span).map(|h| h.entry_key()) {
                    keys.sort_by(|a, b| noeta_ast::map_key_order(a, b, key));
                }
                // A string key becomes a fresh string value; an extern key a fresh extern value
                // (its box cloned — a key is a snapshot); a packed key rebuilds its struct value
                // from the content snapshot (P-PKEY).
                Ok(Value::list(
                    keys.into_iter()
                        .map(|k| match k {
                            noeta_stdlib::MapKey::Str(s) => Value::string(s.as_str()),
                            noeta_stdlib::MapKey::Int(i) => Value::int(i),
                            noeta_stdlib::MapKey::Extern(e) => Value::extern_value(e),
                            noeta_stdlib::MapKey::Packed(p) => {
                                self.packed_key_value(&p.type_name, &p.fields)
                            }
                        })
                        .collect(),
                ))
            }
            noeta_stdlib::MapMethod::Values => {
                self.stdlib_arity(name, args, 0, span)?;
                // In key order — under the observed key order where the map's key type carries a
                // `u64`, so `values()` and `keys()` line up element for element.
                let values = match self.order_hint(&span).map(|h| h.entry_key()) {
                    Some(key) => {
                        let mut entries = map.map_entries_keyed().expect("map receiver");
                        entries.sort_by(|a, b| noeta_ast::map_key_order(&a.0, &b.0, key));
                        entries.into_iter().map(|(_, v)| v).collect()
                    }
                    None => map.map_values().expect("map receiver"),
                };
                for &element in &values {
                    retain(element);
                }
                Ok(Value::list(values))
            }
            noeta_stdlib::MapMethod::Has => {
                self.stdlib_arity(name, args, 1, span)?;
                // Borrow the key's `&str` (or probe through the extern contract) — no clone.
                let present = self.map_probe(map, args[0], name, span)?.is_some();
                Ok(Value::bool(present))
            }
            noeta_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.map_update_key(false, args[0], name, span)?;
                let mut new = map.map_entries_keyed().expect("map receiver");
                new.retain(|(k, _)| *k != key);
                new.push((key, args[1]));
                // The receiver is borrowed (untouched); the new map is a fresh owner, so retain each
                // value it ends up holding exactly once. A displaced/absent value is simply not in
                // `new`, so it keeps only the receiver's reference — no leak, no double-free.
                for &(_, value) in &new {
                    retain(value);
                }
                Ok(Value::map_keyed(new))
            }
            noeta_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.map_update_key(false, args[0], name, span)?;
                let mut new = map.map_entries_keyed().expect("map receiver");
                new.retain(|(k, _)| *k != key);
                for &(_, value) in &new {
                    retain(value);
                }
                Ok(Value::map_keyed(new))
            }
            noeta_stdlib::MapMethod::GetOr => {
                self.stdlib_arity(name, args, 2, span)?;
                // Borrow the key's `&str` (or probe through the extern contract) — no clone.
                let found = self.map_probe(map, args[0], name, span)?;
                // Hit: the value is borrowed from the map. Miss: `default` is borrowed from the
                // caller's argument register. Either way the result register is a new owner.
                let out = found.unwrap_or(args[1]);
                retain(out);
                Ok(out)
            }
            noeta_stdlib::MapMethod::Get => {
                self.stdlib_arity(name, args, 1, span)?;
                // Borrow the key's `&str` (or probe through the extern contract) — no clone.
                match self.map_probe(map, args[0], name, span)? {
                    // Hit: the value is borrowed from the map; the resulting `some(v)` owns it.
                    Some(value) => {
                        retain(value);
                        Ok(make_some(value))
                    }
                    None => Ok(make_none()),
                }
            }
        }
    }

    /// Apply an in-place map update (`set`/`remove`) to a **consumed** map receiver (Phase 5.1c): the
    /// caller has already taken the receiver's single reference out of its register. When uniquely
    /// owned (`refcount == 1`) the backing buffer is mutated in place — O(1) — and the displaced value
    /// (if any) fires its destructor now via `release_value`, matching the copy-and-reassign baseline
    /// (which releases it when the old map dies at the reassignment). An aliased map copies (preserving
    /// the other owner's view), then drops the consumed reference. Run under miri to validate refcounts.
    /// Apply an in-place list `set(index, value)` to a **consumed** list receiver (the caller has
    /// taken its single reference out of the register). When uniquely owned (`refcount == 1`) the slot
    /// is overwritten in place — O(1), the displaced element released — otherwise the list copies
    /// (preserving an alias), then the consumed reference is dropped. An out-of-range index is E0016.
    pub(crate) fn list_set_in_place(
        &mut self,
        list: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let i = self.stdlib_int("set", args[0], span)?;
        let len = list.list_len().unwrap_or(0);
        if i < 0 || i as usize >= len {
            release(list);
            return Err(self.error(
                DiagnosticCode::IndexOutOfBounds,
                span,
                format!("index {i} out of bounds for list of length {len}"),
            ));
        }
        // Sole owner: overwrite the slot in place — O(word_count) for a packed list (its primitives
        // copied into the buffer, P-PACK 2.6) or O(1) pointer-swap for a boxed one. An aliased list
        // (or a packed element that does not pack) copies via `call_list_method` (still flat for a
        // packed receiver) and then drops the consumed reference.
        if list.is_uniquely_owned() {
            if list.is_packed_list() {
                if list.packed_set_in_place(i as usize, args[1]) {
                    return Ok(list);
                }
                // element did not pack (impossible for a well-typed `List<packed>`) — copy below.
            } else {
                let value = args[1];
                retain(value);
                let old = list.list_replace_slot(i as usize, value);
                self.release_value(old);
                return Ok(list);
            }
        }
        // Aliased (or a packed pack-failure): copy via the ordinary method, then drop the consumed
        // reference.
        let new = self.call_list_method(list, noeta_stdlib::ListMethod::Set, "set", args, span)?;
        release(list);
        Ok(new)
    }

    pub(crate) fn map_update_in_place(
        &mut self,
        map: Value,
        method: noeta_stdlib::MapMethod,
        name: &str,
        args: &[Value],
        consume_key: bool,
        span: Span,
    ) -> Result<Value, Abort> {
        if map.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_map_method(map, method, name, args, span)?;
            release(map);
            return Ok(new);
        }
        match method {
            noeta_stdlib::MapMethod::Set => {
                self.stdlib_arity(name, args, 2, span)?;
                let key = self.map_update_key(consume_key, args[0], name, span)?;
                let value = args[1];
                // The map gains an owned reference to the new value.
                retain(value);
                if let Some(old) = map.map_insert(key, value) {
                    self.release_value(old);
                }
                Ok(map)
            }
            noeta_stdlib::MapMethod::Remove => {
                self.stdlib_arity(name, args, 1, span)?;
                let key = self.map_update_key(consume_key, args[0], name, span)?;
                let removed = match &key {
                    noeta_stdlib::MapKey::Str(k) => map.map_remove(k.as_str()),
                    noeta_stdlib::MapKey::Extern(e) => map.map_remove_extern(&**e),
                    owned @ (noeta_stdlib::MapKey::Int(_) | noeta_stdlib::MapKey::Packed(_)) => {
                        map.map_remove_key(owned)
                    }
                };
                if let Some(old) = removed {
                    self.release_value(old);
                }
                Ok(map)
            }
            // Only `set`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update map method on the in-place path"),
        }
    }

    /// Extract the owned [`noeta_stdlib::MapKey`] for a map `set`/`remove`. A string key keeps
    /// its P-SSO paths exactly: when `consume_key` is set (the compiler proved the key a
    /// single-use temporary) and the value is a sole-owned string, **move** its buffer out; else
    /// clone (allocation-free for inline ≤ 24-byte content). A key-capable extern value snapshots
    /// via `clone_box` (extern-types X4). Anything else raises the shared map-key error.
    fn map_update_key(
        &mut self,
        consume_key: bool,
        key: Value,
        _name: &str,
        span: Span,
    ) -> Result<noeta_stdlib::MapKey, Abort> {
        if consume_key && key.is_string() && key.is_uniquely_owned() {
            Ok(noeta_stdlib::MapKey::Str(key.take_string_in_place()))
        } else if let Some(k) = key.as_compact_string() {
            Ok(noeta_stdlib::MapKey::Str(k))
        } else if let Some(i) = key.as_int() {
            // P-PKEY S4: an int key (immediate or boxed) — the zero-allocation kind.
            Ok(noeta_stdlib::MapKey::Int(i))
        } else if key.is_extern() && key.with_extern(noeta_stdlib::map_key::extern_key_capable) {
            Ok(key.with_extern(|e| {
                noeta_stdlib::MapKey::Extern(noeta_stdlib::ExternBox(e.clone_box()))
            }))
        } else if let Some(k) = key.packed_map_key() {
            // P-PKEY: a key-capable `@packed` struct snapshots its content.
            Ok(k)
        } else {
            let error = noeta_stdlib::map_key::map_key_error(key.type_name());
            Err(self.std_dispatch_error(error, span))
        }
    }

    /// Rebuild a packed key's struct value (P-PKEY) — the `keys()` direction. The key's content
    /// snapshot carries the field values; the canonical interned shape comes from the module's
    /// shape table by name (a key of type `T` implies `T` is declared in this module).
    fn packed_key_value(&self, type_name: &str, fields: &[noeta_stdlib::PackedKeyField]) -> Value {
        let shape = self
            .module
            .shapes
            .iter()
            .find(|s| s.kind == noeta_object::ShapeKind::Struct && s.name == type_name)
            .unwrap_or_else(|| panic!("packed key type `{type_name}` must be declared"));
        let shape = noeta_object::intern_shape(shape.clone());
        let slots = fields
            .iter()
            .map(|f| match f {
                noeta_stdlib::PackedKeyField::Int(i) => Value::int(*i),
                noeta_stdlib::PackedKeyField::Bool(b) => Value::bool(*b),
                noeta_stdlib::PackedKeyField::Struct(name, inner) => {
                    self.packed_key_value(name, inner)
                }
            })
            .collect();
        Value::object(shape, slots)
    }

    /// Probe a map by a key argument — a borrowed `&str` (no clone) or the extern contract
    /// (no boxing). A non-key value raises the shared map-key error.
    fn map_probe(
        &mut self,
        map: Value,
        key: Value,
        _name: &str,
        span: Span,
    ) -> Result<Option<Value>, Abort> {
        if let Some(found) = key.with_str(|k| map.map_get(k)) {
            return Ok(found);
        }
        if let Some(i) = key.as_int() {
            return Ok(map.map_get_key(&noeta_stdlib::MapKey::Int(i)));
        }
        if key.is_extern() && key.with_extern(noeta_stdlib::map_key::extern_key_capable) {
            return Ok(key.with_extern(|e| map.map_get_extern(e)));
        }
        // P-PKEY: probe by the packed content snapshot. Builds the key (a few plain words) —
        // fine for a cold probe; a hot packed-key loop goes through the same shape either way.
        if let Some(k) = key.packed_map_key() {
            return Ok(map.map_get_key(&k));
        }
        let error = noeta_stdlib::map_key::map_key_error(key.type_name());
        Err(self.std_dispatch_error(error, span))
    }

    /// In-place `add`/`remove` for a reuse-marked set self-update (`s = s.add(x)` / `s = s.remove(x)`).
    /// The receiver has been consumed from its register by the dispatch above. A uniquely-owned set
    /// mutates its canonical buffer in place via a binary search (the displaced element of a `remove`,
    /// or nothing for `add`, releases now — matching the copy baseline, which drops the old set); an
    /// aliased set copies through the ordinary method so the other owner's view is preserved.
    pub(crate) fn set_update_in_place(
        &mut self,
        set: Value,
        method: noeta_stdlib::SetMethod,
        name: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if set.refcount() != 1 {
            // Aliased: copy, then release the reference we consumed from the receiver register.
            let new = self.call_set_method(set, method, name, args, span)?;
            release(set);
            return Ok(new);
        }
        if let Err(err) = self.stdlib_arity(name, args, 1, span) {
            release(set);
            return Err(err);
        }
        let target = args[0];
        // A target not orderable against the set's class behaves exactly as the copy path: `add`
        // raises the unorderable error, `remove` finds nothing (a no-op). An empty set is orderable
        // with anything, so a first-element probe of `None` (empty) takes the in-place path.
        let orderable = set
            .set_first()
            .is_none_or(|first| noeta_value::set_order(first, target).is_some());
        match method {
            noeta_stdlib::SetMethod::Add => {
                if !orderable {
                    release(set);
                    let error = noeta_stdlib::unorderable_error(name);
                    return Err(self.std_dispatch_error(error, span));
                }
                // The set gains an owned reference only when the element is newly inserted.
                if set.set_insert_sorted(target) {
                    retain(target);
                }
                Ok(set)
            }
            noeta_stdlib::SetMethod::Remove => {
                if orderable && let Some(old) = set.set_remove_sorted(target) {
                    self.release_value(old);
                }
                Ok(set)
            }
            // Only `add`/`remove` are routed to the in-place path by the dispatch guard.
            _ => unreachable!("non-update set method on the in-place path"),
        }
    }

    /// Enforce a collection method's arity, raising the shared `noeta-stdlib` arity error.
    fn stdlib_arity(
        &mut self,
        name: &str,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            let error = noeta_stdlib::arity_error(name, expected, args.len());
            Err(self.std_dispatch_error(error, span))
        }
    }

    /// Accept `min..=max` arguments — a collection method with a trailing-optional parameter
    /// (`slice(start, end?)`, `join(sep?)`). The range twin of [`Self::stdlib_arity`].
    pub(crate) fn stdlib_arity_range(
        &mut self,
        name: &str,
        args: &[Value],
        min: usize,
        max: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if (min..=max).contains(&args.len()) {
            Ok(())
        } else {
            let error = noeta_stdlib::arity_error(name, max, args.len());
            Err(self.std_dispatch_error(error, span))
        }
    }

    /// Read a string argument for a collection method, raising the shared `noeta-stdlib` type error.
    fn stdlib_string(&mut self, name: &str, value: Value, span: Span) -> Result<String, Abort> {
        match value.as_string() {
            Some(s) => Ok(s),
            None => {
                let error = noeta_stdlib::type_error(name, "string");
                Err(self.std_dispatch_error(error, span))
            }
        }
    }

    /// Read an int argument for a collection method, raising the shared `noeta-stdlib` type error.
    /// `as_int` is `None` for a float, so `slice(1.0, 2)` is a type error — matching the
    /// tree-walker, which accepts only `Value::Int`.
    pub(crate) fn stdlib_int(
        &mut self,
        name: &str,
        value: Value,
        span: Span,
    ) -> Result<i64, Abort> {
        match value.as_int() {
            Some(i) => Ok(i),
            None => {
                let error = noeta_stdlib::type_error(name, "int");
                Err(self.std_dispatch_error(error, span))
            }
        }
    }

    /// Read an **optional** int argument at `index`, falling back to `default` when it is absent —
    /// the trailing-optional-parameter reader (`slice`'s `end?`). A present non-int is a type error.
    pub(crate) fn stdlib_opt_int(
        &mut self,
        name: &str,
        args: &[Value],
        index: usize,
        default: i64,
        span: Span,
    ) -> Result<i64, Abort> {
        match args.get(index) {
            None => Ok(default),
            Some(&value) => self.stdlib_int(name, value, span),
        }
    }
}
