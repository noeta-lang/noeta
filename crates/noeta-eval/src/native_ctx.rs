//! The reference interpreter's [`NativeCtx`] implementation (higher-order-abi H0) — the
//! tree-walker twin of the VM's `native_ctx.rs`. Same per-call slot-table shape; here the values
//! are `Rc`-backed clones, so ownership is automatic and the table exists for representation
//! parity (a shared dispatch addresses slots identically in both backends — that is what makes
//! the differential structural).

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_stdlib::{
    CtxError, CtxOut, CtxResult, ExternIo, NativeCtx, NativeOut, NativeValue, PackedView, Retained,
    Scalar, Slot, StdError,
};

use crate::value::ListRepr;
use crate::{
    Eval, Interpreter, ObjectValue, Unwind, Value, materialize_ext, materialize_native,
    scalar_to_value, std_error_code, value_to_native_deep, value_to_scalar,
};

pub(crate) struct EvalCtx<'i> {
    interp: &'i mut Interpreter,
    /// The slot table (the VM twin's entries own manual references; here a slot owns its clone).
    /// Entries below `seeded` are the call's arguments — mirroring the VM's borrowed-seed prefix:
    /// never freed, `take` clones (parity: a seed slot stays readable after a take).
    slots: Vec<Option<Value>>,
    seeded: u32,
    free_list: Vec<Slot>,
    span: Span,
}

fn bad_slot() -> CtxError {
    CtxError::Std(StdError {
        kind: noeta_stdlib::ErrorKind::UnknownName,
        message: "internal: a native dispatch used a freed slot".to_string(),
    })
}

fn bad_retained() -> CtxError {
    CtxError::Std(StdError {
        kind: noeta_stdlib::ErrorKind::UnknownName,
        message: "internal: a native dispatch used a freed retained handle".to_string(),
    })
}

impl<'i> EvalCtx<'i> {
    pub(crate) fn new(interp: &'i mut Interpreter, args: &[Value], span: Span) -> EvalCtx<'i> {
        EvalCtx {
            interp,
            slots: args.iter().cloned().map(Some).collect(),
            seeded: args.len() as u32,
            free_list: Vec::new(),
            span,
        }
    }

    fn insert(&mut self, value: Value) -> Slot {
        if let Some(slot) = self.free_list.pop() {
            self.slots[slot as usize] = Some(value);
            slot
        } else {
            self.slots.push(Some(value));
            (self.slots.len() - 1) as Slot
        }
    }

    fn get(&self, slot: Slot) -> CtxResult<&Value> {
        self.slots
            .get(slot as usize)
            .and_then(Option::as_ref)
            .ok_or_else(bad_slot)
    }

    pub(crate) fn take(&mut self, slot: Slot) -> CtxResult<Value> {
        // A seed stays readable after a take (the VM's borrowed prefix retains a copy out).
        if slot < self.seeded {
            return Ok(self.slots[slot as usize]
                .clone()
                .expect("seeds are never freed"));
        }
        self.slots
            .get_mut(slot as usize)
            .and_then(Option::take)
            .ok_or_else(bad_slot)
    }
}

impl NativeCtx for EvalCtx<'_> {
    fn host(&mut self) -> &mut dyn noeta_stdlib::Host {
        &mut *self.interp.host
    }

    fn write_stdout(&mut self, text: &str) {
        // The same buffer `Stmt::Echo` appends to — an `io.out` and an `echo` interleave in order.
        self.interp.stdout.push_str(text);
    }

    fn write_stderr(&mut self, text: &str) {
        self.interp.stderr.push_str(text);
    }

    fn render(&mut self, slot: Slot) -> CtxResult<String> {
        // Delegate to the interpreter's `display_value` — the one place `to_string` is consulted for
        // `echo` / interpolation — so `io.outln(x)` renders byte-identically to `echo x`, with no
        // second copy of display logic in the std native. An abort in a user `to_string` becomes the
        // propagation token (the diagnostic is recorded on the interpreter).
        let value = self.get(slot)?.clone();
        match self.interp.display_value(&value, self.span) {
            Ok(text) => Ok(text),
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn view(&mut self, slot: Slot) -> CtxResult<NativeValue> {
        Ok(value_to_native_deep(self.get(slot)?))
    }

    fn intern(&mut self, out: NativeOut) -> CtxResult<Slot> {
        // Shape-relative and work results have no meaning here (`materialize_native` would panic).
        if matches!(
            out,
            NativeOut::Object(_) | NativeOut::Struct { .. } | NativeOut::Spawn(_)
        ) {
            return Err(CtxError::Std(StdError {
                kind: noeta_stdlib::ErrorKind::ArgType,
                message: "internal: a native dispatch interned a shape-relative or spawn result"
                    .to_string(),
            }));
        }
        let value = materialize_native(out);
        Ok(self.insert(value))
    }

    fn free(&mut self, slot: Slot) {
        if slot < self.seeded {
            return; // mirror the VM: seeds are never freed
        }
        if self
            .slots
            .get_mut(slot as usize)
            .and_then(Option::take)
            .is_some()
        {
            self.free_list.push(slot);
        }
    }

    fn call(&mut self, callee: Slot, args: &[Slot]) -> CtxResult<Slot> {
        let callee = self.get(callee)?.clone();
        let mut arg_values = Vec::with_capacity(args.len());
        for &a in args {
            arg_values.push(self.get(a)?.clone());
        }
        match self.interp.call(callee, arg_values, self.span) {
            Ok(result) => Ok(self.insert(result)),
            // The diagnostic is recorded on the interpreter; hand back the propagation token.
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn call_with_element(&mut self, callee: Slot, list: Slot, index: usize) -> CtxResult<Slot> {
        let callee = self.get(callee)?.clone();
        let element = match self.get(list)? {
            Value::List(repr) => repr.get(index),
            _ => None,
        }
        .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("call_with_element", "list")))?;
        match self.interp.call(callee, vec![element], self.span) {
            Ok(result) => Ok(self.insert(result)),
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn list_len(&mut self, list: Slot) -> CtxResult<usize> {
        match self.get(list)? {
            Value::List(repr) => Ok(repr.len()),
            _ => Err(CtxError::Std(noeta_stdlib::type_error("list_len", "list"))),
        }
    }

    fn list_get(&mut self, list: Slot, index: usize) -> CtxResult<Slot> {
        let element = match self.get(list)? {
            Value::List(repr) => repr.get(index),
            _ => None,
        }
        .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("list_get", "list")))?;
        Ok(self.insert(element))
    }

    fn make_list(&mut self, items: &[Slot]) -> CtxResult<Slot> {
        let mut elements = Vec::with_capacity(items.len());
        for &item in items {
            // Spent slot: the owned clone moves into the list (the VM twin moves a reference).
            elements.push(self.take(item)?);
        }
        let list = Value::list(elements);
        Ok(self.insert(list))
    }

    fn spawn_io(&mut self, io: Box<dyn ExternIo>) -> Slot {
        let id = self.interp.executor.spawn_ext(&mut *self.interp.host, io);
        self.insert(Value::AsyncIo(id))
    }

    fn timer(&mut self, ms: u64) -> Slot {
        let deadline = self.interp.executor.now() + ms;
        self.insert(Value::Timer(deadline))
    }

    fn poll(&mut self, future: Slot) -> CtxResult<Option<Slot>> {
        let value = self.get(future)?.clone();
        // A combinator (`all`/`race`/`map_bounded`) polling a task handle the user already
        // cancelled fails loudly (Track A.8, E0056) rather than spinning to a deadlock — the same
        // contract `.await` enforces. The VM's mirror.
        if self.interp.handle_cancelled(&value) {
            let _ = self.interp.runtime_error(
                noeta_diagnostics::DiagnosticCode::AwaitCancelled,
                self.span,
                "cannot await a cancelled task; use `.join()` to observe the cancelled outcome"
                    .to_string(),
            );
            return Err(CtxError::Abort);
        }
        match self.interp.poll_once(&value, self.span) {
            Ok(Some(result)) => {
                // A borrowed seed cannot be spent in place — fresh slot, like the VM.
                if future < self.seeded {
                    return Ok(Some(self.insert(result)));
                }
                // Ready spends the future slot; the result takes over its index in place (the
                // VM twin's table-reclaim semantics).
                self.slots[future as usize] = Some(result);
                Ok(Some(future))
            }
            Ok(None) => Ok(None),
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn drive(&mut self, future: Slot) -> CtxResult<Slot> {
        let value = self.get(future)?.clone();
        match self.interp.drive_future(value, self.span) {
            Ok(result) => {
                if future < self.seeded {
                    return Ok(self.insert(result));
                }
                // Spent like a ready poll: the result takes over the future's index in place.
                self.slots[future as usize] = Some(result);
                Ok(future)
            }
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn cancel(&mut self, future: Slot) -> CtxResult<()> {
        let future = self.get(future)?.clone();
        self.interp.cancel_task(&future);
        Ok(())
    }

    fn advance_tasks(&mut self) -> CtxResult<bool> {
        self.interp
            .poll_all_scopes_round(self.span)
            .map_err(|_| CtxError::Abort)
    }

    fn advance_clock(&mut self) -> Option<u64> {
        self.interp.executor.advance()
    }

    // The tree-walker has no OS-thread isolates, hence no external wake source: the generation is
    // constant and a stalled drive loop is a genuine deadlock immediately — exactly the deadlock
    // condition its hand-written `all`/`race`/`map_bounded` arms used (no isolate term).
    fn wake_generation(&mut self) -> u64 {
        0
    }

    fn wait_external_wake(&mut self, _generation: u64) -> bool {
        false
    }

    fn is_list(&mut self, slot: Slot) -> CtxResult<bool> {
        Ok(matches!(self.get(slot)?, Value::List(_)))
    }

    fn type_name(&mut self, slot: Slot) -> CtxResult<&'static str> {
        Ok(self.get(slot)?.type_name())
    }

    fn option_payload(&mut self, slot: Slot) -> CtxResult<Option<Slot>> {
        let payload = match self.get(slot)? {
            Value::Enum(e) if e.enum_name == "Option" && e.variant == "some" => {
                e.data.first().cloned().expect("some carries a payload")
            }
            _ => return Ok(None),
        };
        Ok(Some(self.insert(payload)))
    }

    fn values_equal(&mut self, a: Slot, b: Slot) -> CtxResult<bool> {
        // The language's `==` verbatim (the tree-walker's own Eq rung over borrowed values).
        let av = self.get(a)?.clone();
        let bv = self.get(b)?.clone();
        Ok(crate::ops::value_eq(&av, &bv))
    }

    fn with_extern(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&dyn noeta_stdlib::ExternValue),
    ) -> CtxResult<()> {
        match self.get(slot)? {
            Value::Extern(cell) => {
                f(&**cell.borrow());
                Ok(())
            }
            _ => Err(CtxError::Std(noeta_stdlib::type_error(
                "with_extern",
                "extern value",
            ))),
        }
    }

    // ----- Class 3 (H4): the interpreter twins of the VM's arena/state fields; entries are
    // `Rc` clones, so ownership is automatic. -----

    fn state(
        &mut self,
        key: &'static str,
        init: fn() -> Box<dyn std::any::Any>,
    ) -> noeta_stdlib::ExtState {
        if let Some((_, state)) = self.interp.ext_state.iter().find(|(k, _)| *k == key) {
            return state.clone();
        }
        let state: noeta_stdlib::ExtState = std::rc::Rc::new(std::cell::RefCell::new(init()));
        self.interp.ext_state.push((key, state.clone()));
        state
    }

    fn capability(&mut self, id: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
        // Mirror of the VM: resolve the provider in this interpreter's registry, ensure its backing
        // state, mint the erased handle (differential parity — same seam, same shape).
        let decl = self.interp.reg().find_capability(id)?;
        let state = self.state(decl.state_key, decl.init);
        Some((decl.build)(state))
    }

    fn capabilities(&mut self, id: std::any::TypeId) -> Vec<Box<dyn std::any::Any>> {
        // Mirror of the VM's plural broker (differential parity): every provider, in order.
        let decls: Vec<_> = self.interp.reg().find_capabilities(id).collect();
        decls
            .into_iter()
            .map(|decl| {
                let state = self.state(decl.state_key, decl.init);
                (decl.build)(state)
            })
            .collect()
    }

    fn retain(&mut self, slot: Slot) -> CtxResult<noeta_stdlib::Retained> {
        let value = self.get(slot)?.clone();
        Ok(if let Some(index) = self.interp.ext_arena_free.pop() {
            self.interp.ext_arena[index as usize] = Some(value);
            index
        } else {
            self.interp.ext_arena.push(Some(value));
            (self.interp.ext_arena.len() - 1) as noeta_stdlib::Retained
        })
    }

    fn retained_get(&mut self, retained: noeta_stdlib::Retained) -> CtxResult<Slot> {
        let value = self
            .interp
            .ext_arena
            .get(retained as usize)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(bad_retained)?;
        Ok(self.insert(value))
    }

    fn retained_set(&mut self, retained: noeta_stdlib::Retained, slot: Slot) -> CtxResult<()> {
        let new = self.get(slot)?.clone();
        let old = match self.interp.ext_arena.get_mut(retained as usize) {
            Some(entry @ Some(_)) => entry.replace(new).expect("checked above"),
            _ => return Err(bad_retained()),
        };
        // Destructor-aware, like the VM's `release_value`: this may be the old value's last
        // reference, and its `destruct` must fire now (deterministic destruction).
        self.interp.destroy_value(old);
        Ok(())
    }

    fn release_retained(&mut self, retained: noeta_stdlib::Retained) {
        if let Some(value) = self
            .interp
            .ext_arena
            .get_mut(retained as usize)
            .and_then(Option::take)
        {
            self.interp.destroy_value(value);
            self.interp.ext_arena_free.push(retained);
        }
    }

    // The tree-walker never takes the inlined arena-read fast path — every declared
    // `arena_getter` method goes through the full ctx dispatch here. That asymmetry is an
    // ORACLE: the VM inlines the read while the gate is open, so the differential proves the
    // extension's contract ("the fast path and the full dispatch behave identically whenever
    // the gate is open") on every reactive/cell fixture. Gates are therefore meaningless here.
    fn set_read_gate(&mut self, _type_name: &'static str, _open: bool) {}

    fn run_thunk(&mut self, body: Retained) -> CtxResult<()> {
        let callee = self
            .interp
            .ext_arena
            .get(body as usize)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(bad_retained)?;
        match self.interp.call(callee, Vec::new(), self.span) {
            Ok(result) => {
                self.interp.destroy_value(result);
                Ok(())
            }
            Err(_) => Err(CtxError::Abort),
        }
    }

    fn call_thunk_into(&mut self, body: Retained, dest: Retained) -> CtxResult<()> {
        let callee = self
            .interp
            .ext_arena
            .get(body as usize)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(bad_retained)?;
        let result = match self.interp.call(callee, Vec::new(), self.span) {
            Ok(result) => result,
            Err(_) => return Err(CtxError::Abort),
        };
        let old = match self.interp.ext_arena.get_mut(dest as usize) {
            Some(entry @ Some(_)) => entry.replace(result).expect("checked above"),
            _ => return Err(bad_retained()),
        };
        self.interp.destroy_value(old);
        Ok(())
    }

    // ----- The raw-buffer ABI (package-manager N3.4): the tree-walker twins. -----

    fn with_packed(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&PackedView, &[u8]),
    ) -> CtxResult<bool> {
        match self.get(slot)? {
            Value::List(ListRepr::Packed(p)) => {
                f(&p.seam_view(), p.raw());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn with_packed_mut(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&PackedView, &mut [u8]),
    ) -> CtxResult<Option<Slot>> {
        if !matches!(self.get(slot)?, Value::List(ListRepr::Packed(_))) {
            return Ok(None);
        }
        // `take` spends a non-seed slot / clones a seed (the VM contract); `Rc::make_mut` inside
        // `mutate_bytes` then supplies the copy-on-write for free — in place iff sole owner.
        let Value::List(ListRepr::Packed(mut p)) = self.take(slot)? else {
            unreachable!("checked packed above")
        };
        p.mutate_bytes(f);
        Ok(Some(self.insert(Value::List(ListRepr::Packed(p)))))
    }

    fn make_packed_like(&mut self, like: Slot, bytes: Vec<u8>) -> CtxResult<Slot> {
        let list = match self.get(like)? {
            Value::List(ListRepr::Packed(p)) => p.like(bytes),
            _ => {
                return Err(CtxError::Std(noeta_stdlib::type_error(
                    "make_packed_like",
                    "packed list",
                )));
            }
        };
        Ok(self.insert(Value::List(ListRepr::Packed(list))))
    }

    fn make_packed(&mut self, type_name: &str, bytes: Vec<u8>) -> CtxResult<Slot> {
        // Resolve the element schema BY NAME — the tree-walker twin of the VM's interned
        // `packed_schemas` scan. The layout is the checker's, threaded in at `run_ir` start; the def
        // is registry-or-scope (`resolve_packed_schema` → `packed_type_def`), so a native `@packed`
        // struct's qualified name resolves even though only its short name is scope-bound.
        let Some(layout) = self.interp.packed_type_layouts.get(type_name).cloned() else {
            return Err(CtxError::Std(StdError {
                kind: noeta_stdlib::ErrorKind::UnknownName,
                message: format!(
                    "make_packed: no interned packed schema for `{type_name}` — it must be a \
                     `@packed` struct reachable in the compiled unit, named by its qualified identity"
                ),
            }));
        };
        let Some(schema) = self.interp.resolve_packed_schema(&layout) else {
            return Err(CtxError::Std(StdError {
                kind: noeta_stdlib::ErrorKind::UnknownName,
                message: format!(
                    "make_packed: the packed layout for `{type_name}` did not resolve to a packable \
                     element schema"
                ),
            }));
        };
        if schema.byte_size == 0 || !bytes.len().is_multiple_of(schema.byte_size) {
            return Err(CtxError::Std(StdError {
                kind: noeta_stdlib::ErrorKind::ArgType,
                message: format!(
                    "make_packed: buffer of {} bytes is not a whole number of `{type_name}` \
                     elements ({} bytes each)",
                    bytes.len(),
                    schema.byte_size
                ),
            }));
        }
        Ok(self.insert(Value::packed_list_from(schema, bytes)))
    }

    fn object_scalars_at(
        &mut self,
        list: Slot,
        index: usize,
        out: &mut Vec<Scalar>,
    ) -> CtxResult<bool> {
        out.clear();
        let element = match self.get(list)? {
            Value::List(repr) => repr.get(index),
            _ => None,
        }
        .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("object_scalars_at", "list")))?;
        let Value::Object(obj) = element else {
            return Ok(false);
        };
        for s in obj.slots.borrow().iter() {
            match value_to_scalar(s) {
                Some(scalar) => out.push(scalar),
                None => {
                    out.clear();
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn make_object_like_element(
        &mut self,
        list: Slot,
        index: usize,
        fields: &[Scalar],
    ) -> CtxResult<Slot> {
        let element = match self.get(list)? {
            Value::List(repr) => repr.get(index),
            _ => None,
        };
        let built = match element {
            Some(Value::Object(obj)) if obj.slots.borrow().len() == fields.len() => {
                Value::Object(std::rc::Rc::new(ObjectValue::new(
                    std::rc::Rc::clone(&obj.def),
                    fields.iter().map(|&s| scalar_to_value(s)).collect(),
                )))
            }
            _ => {
                return Err(CtxError::Std(noeta_stdlib::type_error(
                    "make_object_like_element",
                    "object of matching field count",
                )));
            }
        };
        Ok(self.insert(built))
    }

    fn packed_element_fields(
        &mut self,
        slot: Slot,
    ) -> CtxResult<Option<Vec<noeta_stdlib::PackedField>>> {
        // The element bundle methods' width source (scalar-unification slice 3), the tree-walker twin
        // of the VM's `packed_schemas` scan: the interpreter records every `@packed` struct's field
        // type names, so the value's own `def` name recovers the exact kinds a single struct value's
        // boxed scalars erase.
        let name = match self.get(slot)? {
            Value::Object(obj) => obj.def.name().to_string(),
            _ => return Ok(None),
        };
        Ok(self.interp.packed_field_kinds(&name))
    }

    // ----- scheduler-service sub-capabilities: the interpreter is its own provider (returns `self`).
    // `HotReload` is not overridden (the tree-walker is never under `serve --watch`) — it takes the
    // trait defaults (0/None), the same inert answers the flat method gave before the split. -----

    fn task_context(&mut self) -> &mut dyn noeta_stdlib::TaskContext {
        self
    }

    fn future_tracing(&mut self) -> &mut dyn noeta_stdlib::FutureTracing {
        self
    }

    fn hot_reload(&mut self) -> &mut dyn noeta_stdlib::HotReload {
        self
    }
}

// task-local context (native-otel T5a): thin views over `Interpreter::ctx_current`.
impl noeta_stdlib::TaskContext for EvalCtx<'_> {
    fn top(&mut self) -> Option<u64> {
        self.interp.ctx_current.last().copied()
    }

    fn push(&mut self, v: u64) {
        self.interp.ctx_current.push(v);
    }

    fn pop(&mut self, v: u64) {
        if self.interp.ctx_current.last() == Some(&v) {
            self.interp.ctx_current.pop();
        }
    }

    fn swap(&mut self, ctx: Vec<u64>) -> Vec<u64> {
        std::mem::replace(&mut self.interp.ctx_current, ctx)
    }
}

impl noeta_stdlib::FutureTracing for EvalCtx<'_> {
    fn trace(&mut self, future: Slot, span: u64) -> CtxResult<bool> {
        let value = self.get(future)?;
        // Only a step future is traceable — the same line the VM draws, so telemetry parity
        // holds for the fallback too.
        if !matches!(value, Value::Future(_)) {
            return Ok(false);
        }
        let mut context = self.interp.ctx_current.clone();
        context.push(span);
        self.interp.traced_futures.push(crate::TracedFuture {
            future: value.clone(),
            context,
            span,
        });
        Ok(true)
    }
}

// The tree-walker never runs under `serve --watch`, so hot-reload takes the inert defaults.
impl noeta_stdlib::HotReload for EvalCtx<'_> {}

impl Interpreter {
    /// Call a registered extern type's **higher-order method** (higher-order-abi H4) — the
    /// tree-walker twin of the VM's `call_ctx_type_method`. The receiver rides as slot 0.
    pub(crate) fn call_ctx_type_method(
        &mut self,
        type_name: &'static str,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        // Bound before the closures so they capture the registry, not `self` (instance-registry
        // IR3) — the tree-walker twin of the VM's binding. The `static_dispatch_ctx_method` fast
        // path stays on the global; only the dyn-table fallback consults `reg`.
        let reg = self.reg();
        self.ctx_receiver_call(
            recv,
            args,
            span,
            || format!("{type_name}.{method}"),
            // Same monomorphized route as the VM (identical fn either way — the instantiation
            // only decides inlining).
            |ctx, arg_slots| {
                noeta_stdlib::registry::static_dispatch_ctx_method(
                    type_name, method, ctx, 0, arg_slots,
                )
                .unwrap_or_else(|| reg.dispatch_ctx_method(type_name, method, ctx, 0, arg_slots))
            },
            || {
                reg.find_type_ctx_method(type_name, method)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit))
            },
        )
    }

    /// Call a **trait** method (a native default body, slice 2; or a kernel-trait method since the
    /// ExtBundle→ExtTrait fold-in, slice 4) — the tree-walker twin of the VM's `call_trait_method`:
    /// the compiler baked the `(trait, method)` route, receiver rides as slot 0. `trait_q` is the
    /// trait's qualified identity.
    pub(crate) fn call_trait_method(
        &mut self,
        trait_q: &str,
        method: &str,
        recv: Value,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        // Bound before the closures so they capture the registry, not `self` (IR3).
        let reg = self.reg();
        self.ctx_receiver_call(
            recv,
            args,
            span,
            || format!("trait {trait_q}.{method}"),
            |ctx, arg_slots| reg.dispatch_trait_method(trait_q, method, ctx, 0, arg_slots),
            || {
                reg.find_trait_qualified(trait_q)
                    .and_then(|t| t.methods.iter().find(|m| m.sig.name == method))
                    .map(|m| m.sig.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit))
            },
        )
    }

    /// The shared receiver-seeded ctx-call shape (extern-type ctx methods + bundle methods) —
    /// the tree-walker twin of the VM's `ctx_receiver_call`.
    fn ctx_receiver_call(
        &mut self,
        recv: Value,
        args: &[Value],
        span: Span,
        label: impl FnOnce() -> String,
        dispatch: impl FnOnce(&mut EvalCtx<'_>, &[Slot]) -> Result<CtxOut, CtxError>,
        ret: impl FnOnce() -> noeta_stdlib::RetTy,
    ) -> Eval<Value> {
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend_from_slice(args);
        let mut ctx = EvalCtx::new(self, &all, span);
        let arg_slots: Vec<Slot> = (1..all.len() as Slot).collect();
        let outcome = dispatch(&mut ctx, &arg_slots);
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{}` returned a freed slot", label()),
                    )),
                }
            }
            // A retained arena entry as the result (a `get` over a stable cell).
            Ok(CtxOut::Retained(retained)) => {
                drop(ctx);
                match self
                    .ext_arena
                    .get(retained as usize)
                    .and_then(Option::as_ref)
                    .cloned()
                {
                    Some(value) => Ok(value),
                    None => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        "internal: a native dispatch returned a freed retained handle".to_string(),
                    )),
                }
            }
            Ok(CtxOut::Out(out)) => {
                drop(ctx);
                Ok(materialize_ext(out, ret(), &all))
            }
            Err(CtxError::Std(e)) => {
                drop(ctx);
                Err(self.runtime_error(std_error_code(e.kind), span, e.message))
            }
            // The abort's diagnostic is already recorded (the re-entry recorded it); propagate.
            Err(CtxError::Abort) => Err(Unwind::Abort),
        }
    }

    /// Call a registered **higher-order** native function (higher-order-abi H0) — the tree-walker
    /// twin of the VM's `call_ctx_function`. The dispatch body is shared, so the backends agree
    /// by construction.
    pub(crate) fn call_ctx_function(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Eval<Value> {
        // Bound before `ctx` takes `&mut self` (IR3): `reg` is `&'static`, so it survives the
        // borrow and the fallback dispatch routes through this interpreter's registry.
        let reg = self.reg();
        let mut ctx = EvalCtx::new(self, args, span);
        let arg_slots: Vec<Slot> = (0..args.len() as Slot).collect();
        let outcome =
            noeta_stdlib::registry::static_dispatch_ctx(module, func, &mut ctx, &arg_slots)
                .unwrap_or_else(|| reg.dispatch_ctx(module, func, &mut ctx, &arg_slots));
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{module}.{func}` returned a freed slot"),
                    )),
                }
            }
            // A retained arena entry as the result (a `get` over a stable cell).
            Ok(CtxOut::Retained(retained)) => {
                drop(ctx);
                match self
                    .ext_arena
                    .get(retained as usize)
                    .and_then(Option::as_ref)
                    .cloned()
                {
                    Some(value) => Ok(value),
                    None => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        "internal: a native dispatch returned a freed retained handle".to_string(),
                    )),
                }
            }
            Ok(CtxOut::Out(out)) => {
                drop(ctx);
                let ret = reg
                    .find_ctx_function(module, func)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit));
                Ok(materialize_ext(out, ret, args))
            }
            Err(CtxError::Std(e)) => {
                drop(ctx);
                Err(self.runtime_error(std_error_code(e.kind), span, e.message))
            }
            // The abort's diagnostic is already recorded (the re-entry recorded it); propagate.
            Err(CtxError::Abort) => Err(Unwind::Abort),
        }
    }
}
