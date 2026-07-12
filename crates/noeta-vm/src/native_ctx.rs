//! The VM's [`NativeCtx`] implementation (higher-order-abi H0): how a registered higher-order
//! native function re-enters the VM. A `VmCtx` is a per-call wrapper over `&mut Vm` plus a
//! **slot table** — each `Some` entry owns exactly one reference to its value (retained on
//! insert, released on [`NativeCtx::free`] or at drop). Centralizing the ownership here is the
//! point: the hand-written `Builtin` arms this seam replaces leaked twice during the http-server
//! arc precisely because every one re-derived the retain/release choreography by hand. A shared
//! dispatch cannot leak a slot — the table's `Drop` releases whatever the dispatch forgot.

use noeta_diagnostics::DiagnosticCode;
use noeta_gc::retain;
use noeta_span::Span;
use noeta_stdlib::{
    CtxError, CtxOut, CtxResult, ExternIo, NativeCtx, NativeOut, NativeValue, PackedField,
    PackedView, Retained, Scalar, Slot, StdError,
};
use noeta_value::Value;

use crate::values::{materialize_ext, materialize_native};
use crate::{Abort, Poll, Vm, isolate, stdlib_error_code};

pub(crate) struct VmCtx<'c, 'm> {
    vm: &'c mut Vm<'m>,
    /// The slot table. Entries below `seeded` are **borrowed** from the caller's registers (which
    /// outlive the call) — no retain on entry, no release at drop (H5 perf: a hot `set(v)` pays
    /// zero refcount traffic for its receiver + argument). Entries at/above `seeded` own one
    /// reference each; `None` is freed (index reusable via `free_list`, which never holds a seed).
    slots: Vec<Option<Value>>,
    seeded: u32,
    free_list: Vec<Slot>,
    span: Span,
}

/// Project the VM's interned packed schema onto the seam's neutral [`PackedView`] (package-manager
/// N3.4) — built per `with_packed*` call; a bulk kernel pays it once per *call*, not per element.
fn packed_view(schema: &noeta_object::PackedSchema, buffer_len: usize) -> PackedView {
    PackedView {
        fields: schema.fields.iter().map(packed_field).collect(),
        byte_size: schema.byte_size,
        column: schema.column,
        count: schema.count(buffer_len),
    }
}

fn packed_field(kind: &noeta_object::PackedKind) -> PackedField {
    use noeta_object::PackedKind;
    match kind {
        PackedKind::Int => PackedField::Int,
        PackedKind::Float => PackedField::Float,
        PackedKind::F32 => PackedField::F32,
        PackedKind::Bool => PackedField::Bool,
        PackedKind::Struct(inner) => {
            PackedField::Struct(inner.fields.iter().map(packed_field).collect())
        }
    }
}

fn bad_slot() -> CtxError {
    CtxError::Std(StdError {
        kind: noeta_stdlib::ErrorKind::UnknownName,
        message: "internal: a native dispatch used a freed slot".to_string(),
    })
}

/// An **owned** reference to `list[index]` for the ctx element reads: a boxed list hands out a
/// borrowed reference (retained here), a packed list materializes the element owned (rc 1 —
/// `Value::list_get` is deliberately boxed-only). Errs on a non-list or out-of-range index.
fn element_owned(list: Value, index: usize, op: &str) -> CtxResult<Value> {
    if list.is_packed_list() {
        if index >= list.list_len().unwrap_or(0) {
            return Err(CtxError::Std(noeta_stdlib::type_error(op, "list")));
        }
        return Ok(list.packed_get(index));
    }
    let element = list
        .list_get(index)
        .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error(op, "list")))?;
    retain(element);
    Ok(element)
}

fn bad_retained() -> CtxError {
    CtxError::Std(StdError {
        kind: noeta_stdlib::ErrorKind::UnknownName,
        message: "internal: a native dispatch used a freed retained handle".to_string(),
    })
}

impl<'c, 'm> VmCtx<'c, 'm> {
    /// Wrap the VM for one dispatch, seeding the table with the caller's (borrowed) arguments —
    /// NOT retained: the caller's registers own them and outlive the call (see `seeded`). The
    /// table itself comes from the VM's pool (H5 perf), so a hot dispatch loop runs alloc-free;
    /// re-entrant dispatches simply pop the next spare.
    pub(crate) fn new(vm: &'c mut Vm<'m>, args: &[Value], span: Span) -> VmCtx<'c, 'm> {
        let mut slots = vm.ctx_table_pool.pop().unwrap_or_default();
        slots.extend(args.iter().map(|&a| Some(a)));
        VmCtx {
            vm,
            slots,
            seeded: args.len() as u32,
            free_list: Vec::new(),
            span,
        }
    }

    /// Seed one (borrowed) argument into the table — like `new`'s seeding, not retained. Used by
    /// the type-method entry to place the receiver + args without an intermediate vec. Must only
    /// be called before any non-seed insert (the borrowed prefix is `0..seeded`).
    pub(crate) fn insert_seed(&mut self, borrowed: Value) {
        debug_assert_eq!(self.slots.len(), self.seeded as usize, "seeds come first");
        self.slots.push(Some(borrowed));
        self.seeded += 1;
    }

    /// Store an **owned** reference, minting its slot (freed indices are reused so a long serve
    /// loop's table stays bounded).
    fn insert(&mut self, owned: Value) -> Slot {
        if let Some(slot) = self.free_list.pop() {
            self.slots[slot as usize] = Some(owned);
            slot
        } else {
            self.slots.push(Some(owned));
            (self.slots.len() - 1) as Slot
        }
    }

    /// A borrowed view of a slot's value (the table keeps its reference).
    fn get(&self, slot: Slot) -> CtxResult<Value> {
        self.slots
            .get(slot as usize)
            .copied()
            .flatten()
            .ok_or_else(bad_slot)
    }

    /// Move a slot's value **out** of the table (for the dispatch's `CtxOut::Slot` result — the
    /// one reference transfers to the caller). Taking a *seed* slot retains instead (the table
    /// never owned it; the caller receives a fresh reference).
    pub(crate) fn take(&mut self, slot: Slot) -> CtxResult<Value> {
        if slot < self.seeded {
            let value = self.slots[slot as usize].expect("seeds are never freed");
            retain(value);
            return Ok(value);
        }
        self.slots
            .get_mut(slot as usize)
            .and_then(Option::take)
            .ok_or_else(bad_slot)
    }
}

impl Drop for VmCtx<'_, '_> {
    fn drop(&mut self) {
        // Release every reference the dispatch left behind (forgotten temps — the borrowed seed
        // prefix is the caller's), then hand the emptied table back to the pool.
        let mut slots = std::mem::take(&mut self.slots);
        for value in slots.drain(..).skip(self.seeded as usize).flatten() {
            self.vm.release_value(value);
        }
        // Cap what we keep: a long-lived serve loop's table stays window-sized, but a one-off
        // giant table (a 200k-result collect) is not worth holding onto.
        if slots.capacity() <= 1024 && self.vm.ctx_table_pool.len() < 8 {
            self.vm.ctx_table_pool.push(slots);
        }
    }
}

impl NativeCtx for VmCtx<'_, '_> {
    fn host(&mut self) -> &mut dyn noeta_stdlib::Host {
        &mut *self.vm.host
    }

    fn view(&mut self, slot: Slot) -> CtxResult<NativeValue> {
        Ok(self.get(slot)?.to_native_deep())
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
        Ok(self.insert(materialize_native(out)))
    }

    fn free(&mut self, slot: Slot) {
        // A seed is the caller's — freeing it is a no-op (it dies with the call anyway), and its
        // index never enters the free list (the borrowed prefix must stay intact).
        if slot < self.seeded {
            return;
        }
        if let Some(value) = self.slots.get_mut(slot as usize).and_then(Option::take) {
            self.vm.release_value(value);
            self.free_list.push(slot);
        }
    }

    fn call(&mut self, callee: Slot, args: &[Slot]) -> CtxResult<Slot> {
        let callee = self.get(callee)?;
        // `call_value` consumes its argument vector (the references move into the callee's
        // frame), so retain each table-owned reference on the way in.
        let mut arg_values = Vec::with_capacity(args.len());
        for &a in args {
            let v = self.get(a)?;
            retain(v);
            arg_values.push(v);
        }
        match self.vm.call_value(callee, arg_values, self.span) {
            Ok(result) => Ok(self.insert(result)),
            // The diagnostic is recorded on the VM; hand the dispatch the propagation token.
            Err(Abort) => Err(CtxError::Abort),
        }
    }

    fn call_with_element(&mut self, callee: Slot, list: Slot, index: usize) -> CtxResult<Slot> {
        let callee = self.get(callee)?;
        // An owned reference to `list[index]`: a boxed element is borrowed + retained; a packed
        // element materializes owned (rc 1) — the callee's frame consumes it either way.
        let element = element_owned(self.get(list)?, index, "call_with_element")?;
        // The element rides straight into the callee's frame — no table entry, exactly the fused
        // `list_get` + `call` + `free`.
        match self.vm.call_value(callee, vec![element], self.span) {
            Ok(result) => Ok(self.insert(result)),
            Err(Abort) => Err(CtxError::Abort),
        }
    }

    fn list_len(&mut self, list: Slot) -> CtxResult<usize> {
        self.get(list)?
            .list_len()
            .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("list_len", "list")))
    }

    fn list_get(&mut self, list: Slot, index: usize) -> CtxResult<Slot> {
        // Owned either way (see `element_owned`); the table adopts the reference.
        let element = element_owned(self.get(list)?, index, "list_get")?;
        Ok(self.insert(element))
    }

    fn make_list(&mut self, items: &[Slot]) -> CtxResult<Slot> {
        let mut elements = Vec::with_capacity(items.len());
        for &item in items {
            // The slot is spent: its owned reference moves straight into the list — no RC ops.
            elements.push(self.take(item)?);
        }
        let list = Value::list(elements);
        Ok(self.insert(list))
    }

    fn spawn_io(&mut self, io: Box<dyn ExternIo>) -> Slot {
        let id = self.vm.executor.spawn_ext(&mut *self.vm.host, io);
        self.insert(Value::make_async_io(id))
    }

    fn timer(&mut self, ms: u64) -> Slot {
        self.insert(Value::make_timer(self.vm.executor.now() + ms))
    }

    fn poll(&mut self, future: Slot) -> CtxResult<Option<Slot>> {
        let value = self.get(future)?;
        match self.vm.poll_once(value, self.span) {
            Ok(Poll::Ready(result)) => {
                // A borrowed seed cannot be spent in place (the entry is the caller's) — the
                // result gets a fresh owned slot instead.
                if future < self.seeded {
                    return Ok(Some(self.insert(result)));
                }
                // Ready spends the future slot (never re-polled); the result takes over its
                // index in place — one write, no free-list churn, the table stays hot and tiny.
                let entry = &mut self.slots[future as usize];
                let spent = entry.replace(result).expect("checked by get above");
                self.vm.release_value(spent);
                Ok(Some(future))
            }
            Ok(Poll::Pending) => Ok(None),
            Err(Abort) => Err(CtxError::Abort),
        }
    }

    fn drive(&mut self, future: Slot) -> CtxResult<Slot> {
        let value = self.get(future)?;
        match self.vm.drive_future(value, self.span) {
            Ok(result) => {
                // A borrowed seed cannot be spent in place — fresh owned slot (see `poll`).
                if future < self.seeded {
                    return Ok(self.insert(result));
                }
                // Spent like a ready poll: the result takes over the future's index in place.
                let entry = &mut self.slots[future as usize];
                let spent = entry.replace(result).expect("checked by get above");
                self.vm.release_value(spent);
                Ok(future)
            }
            Err(Abort) => Err(CtxError::Abort),
        }
    }

    fn cancel(&mut self, future: Slot) -> CtxResult<()> {
        let future = self.get(future)?;
        self.vm.cancel_task(future);
        Ok(())
    }

    fn advance_tasks(&mut self) -> CtxResult<bool> {
        // The hot-reload safepoint (server-hmr W1): every ctx-driven loop (the HTTP serve loop)
        // ticks the scheduler each iteration, so a pending swap lands here — before the poll, so
        // the next accepted request already dispatches into the new bodies. One `Option` branch
        // on every run that isn't `serve --watch`.
        if self.vm.hot_mailbox.is_some() {
            self.vm.apply_pending_hotswap();
        }
        self.vm
            .poll_all_scopes_round(self.span)
            .map_err(|Abort| CtxError::Abort)
    }

    fn advance_clock(&mut self) -> Option<u64> {
        self.vm.executor.advance()
    }

    fn wake_generation(&mut self) -> u64 {
        isolate::WAKE.generation()
    }

    fn wait_external_wake(&mut self, generation: u64) -> bool {
        self.vm.isolate_in_flight_wait(generation)
    }

    fn is_list(&mut self, slot: Slot) -> CtxResult<bool> {
        Ok(self.get(slot)?.is_list())
    }

    fn type_name(&mut self, slot: Slot) -> CtxResult<&'static str> {
        Ok(self.get(slot)?.type_name())
    }

    fn option_payload(&mut self, slot: Slot) -> CtxResult<Option<Slot>> {
        let value = self.get(slot)?;
        let is_some = value
            .shape()
            .map(|s| s.name == "Option" && s.variant.as_deref() == Some("some"))
            .unwrap_or(false);
        if !is_some {
            return Ok(None);
        }
        // The payload is shared with the enum; the table takes its own reference.
        let payload = value
            .enum_data()
            .and_then(|d| d.into_iter().next())
            .expect("some carries a payload");
        retain(payload);
        Ok(Some(self.insert(payload)))
    }

    fn with_extern(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&dyn noeta_stdlib::ExternValue),
    ) -> CtxResult<()> {
        let value = self.get(slot)?;
        if !value.is_extern() {
            return Err(CtxError::Std(noeta_stdlib::type_error(
                "with_extern",
                "extern value",
            )));
        }
        value.with_extern(|e| f(e));
        Ok(())
    }

    // ----- Class 3 (H4): per-run state + the retained arena live on the Vm, so they survive
    // across dispatches; the ctx methods are thin views over those fields. -----

    fn state(
        &mut self,
        key: &'static str,
        init: fn() -> Box<dyn std::any::Any>,
    ) -> noeta_stdlib::ExtState {
        if let Some((_, state)) = self.vm.ext_state.iter().find(|(k, _)| *k == key) {
            return state.clone();
        }
        let state: noeta_stdlib::ExtState = std::rc::Rc::new(std::cell::RefCell::new(init()));
        self.vm.ext_state.push((key, state.clone()));
        state
    }

    fn retain(&mut self, slot: Slot) -> CtxResult<noeta_stdlib::Retained> {
        let value = self.get(slot)?;
        // The arena takes its own reference; the slot stays table-owned.
        retain(value);
        Ok(if let Some(index) = self.vm.ext_arena_free.pop() {
            self.vm.ext_arena[index as usize] = Some(value);
            index
        } else {
            self.vm.ext_arena.push(Some(value));
            (self.vm.ext_arena.len() - 1) as noeta_stdlib::Retained
        })
    }

    fn retained_get(&mut self, retained: noeta_stdlib::Retained) -> CtxResult<Slot> {
        let value = self
            .vm
            .ext_arena
            .get(retained as usize)
            .copied()
            .flatten()
            .ok_or_else(bad_retained)?;
        retain(value);
        Ok(self.insert(value))
    }

    fn retained_set(&mut self, retained: noeta_stdlib::Retained, slot: Slot) -> CtxResult<()> {
        let new = self.get(slot)?;
        let Some(entry) = self
            .vm
            .ext_arena
            .get_mut(retained as usize)
            .filter(|e| e.is_some())
        else {
            return Err(bad_retained());
        };
        retain(new);
        let old = entry.replace(new).expect("checked above");
        // Destructor-aware: this may be the old value's last reference.
        self.vm.release_value(old);
        Ok(())
    }

    fn release_retained(&mut self, retained: noeta_stdlib::Retained) {
        if let Some(value) = self
            .vm
            .ext_arena
            .get_mut(retained as usize)
            .and_then(Option::take)
        {
            self.vm.release_value(value);
            self.vm.ext_arena_free.push(retained);
        }
    }

    fn set_read_gate(&mut self, type_name: &'static str, open: bool) {
        let closed = &mut self.vm.ext_closed_gates;
        if open {
            closed.retain(|t| *t != type_name);
        } else if !closed.contains(&type_name) {
            closed.push(type_name);
        }
    }

    fn run_thunk(&mut self, body: Retained) -> CtxResult<()> {
        let callee = self
            .vm
            .ext_arena
            .get(body as usize)
            .copied()
            .flatten()
            .ok_or_else(bad_retained)?;
        // `call_value` borrows the callee (the arena keeps its reference) and returns an owned
        // result — released here, no slot ever minted.
        match self.vm.call_value(callee, Vec::new(), self.span) {
            Ok(result) => {
                self.vm.release_value(result);
                Ok(())
            }
            Err(Abort) => Err(CtxError::Abort),
        }
    }

    fn call_thunk_into(&mut self, body: Retained, dest: Retained) -> CtxResult<()> {
        let callee = self
            .vm
            .ext_arena
            .get(body as usize)
            .copied()
            .flatten()
            .ok_or_else(bad_retained)?;
        let result = match self.vm.call_value(callee, Vec::new(), self.span) {
            Ok(result) => result,
            Err(Abort) => return Err(CtxError::Abort),
        };
        // The owned result moves straight into the cell; the displaced value releases
        // destructor-aware.
        let Some(entry) = self
            .vm
            .ext_arena
            .get_mut(dest as usize)
            .filter(|e| e.is_some())
        else {
            self.vm.release_value(result);
            return Err(bad_retained());
        };
        let old = entry.replace(result).expect("checked above");
        self.vm.release_value(old);
        Ok(())
    }

    // ----- The raw-buffer ABI (package-manager N3.4) -----

    fn with_packed(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&PackedView, &[u8]),
    ) -> CtxResult<bool> {
        let value = self.get(slot)?;
        Ok(value
            .with_packed_ref(|schema, bytes| f(&packed_view(schema, bytes.len()), bytes))
            .is_some())
    }

    fn with_packed_mut(
        &mut self,
        slot: Slot,
        f: &mut dyn FnMut(&PackedView, &mut [u8]),
    ) -> CtxResult<Option<Slot>> {
        let value = self.get(slot)?;
        if !value.is_packed_list() {
            return Ok(None);
        }
        // Proven sole ownership — a table-owned slot holding the only reference: mutate the
        // buffer in place (the P-REUSE shape, zero copy). The slot is spent either way.
        if slot >= self.seeded && value.is_uniquely_owned() {
            let owned = self.take(slot)?;
            owned.packed_mutate_in_place(|schema, bytes| {
                f(&packed_view(schema, bytes.len()), bytes)
            });
            return Ok(Some(self.insert(owned)));
        }
        // Shared (or a borrowed seed): copy-on-write — clone the buffer, mutate the clone. A
        // non-seed input is spent (`free` is a no-op on a seed).
        let (schema, mut bytes) = value.packed_parts().expect("checked packed above");
        f(&packed_view(schema, bytes.len()), &mut bytes);
        self.free(slot);
        Ok(Some(self.insert(Value::packed_list(schema, bytes))))
    }

    fn make_packed_like(&mut self, like: Slot, bytes: Vec<u8>) -> CtxResult<Slot> {
        let value = self.get(like)?;
        let Some(schema) = value.with_packed_ref(|schema, _| schema) else {
            return Err(CtxError::Std(noeta_stdlib::type_error(
                "make_packed_like",
                "packed list",
            )));
        };
        debug_assert!(
            schema.byte_size > 0 && bytes.len().is_multiple_of(schema.byte_size),
            "make_packed_like: a whole number of elements"
        );
        Ok(self.insert(Value::packed_list(schema, bytes)))
    }

    fn object_scalars_at(
        &mut self,
        list: Slot,
        index: usize,
        out: &mut Vec<Scalar>,
    ) -> CtxResult<bool> {
        out.clear();
        let list = self.get(list)?;
        // A packed element materializes once (owned) and is released after the read; a boxed
        // element is read through the borrow — zero refcount traffic either way.
        if list.is_packed_list() {
            if index >= list.list_len().unwrap_or(0) {
                return Err(CtxError::Std(noeta_stdlib::type_error(
                    "object_scalars_at",
                    "list",
                )));
            }
            let element = list.packed_get(index);
            let ok = element.scalar_slots_into(out);
            self.vm.release_value(element);
            return Ok(ok);
        }
        let element = list
            .list_get(index)
            .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("object_scalars_at", "list")))?;
        Ok(element.scalar_slots_into(out))
    }

    fn make_object_like_element(
        &mut self,
        list: Slot,
        index: usize,
        fields: &[Scalar],
    ) -> CtxResult<Slot> {
        let list = self.get(list)?;
        // The shape only — a packed list's schema carries it directly, no materialization.
        let shape = if list.is_packed_list() {
            list.with_packed_ref(|schema, _| schema.shape)
        } else {
            list.list_get(index).and_then(|e| e.shape())
        };
        let Some(shape) = shape.filter(|s| s.fields.len() == fields.len()) else {
            return Err(CtxError::Std(noeta_stdlib::type_error(
                "make_object_like_element",
                "object of matching field count",
            )));
        };
        let slots = fields
            .iter()
            .map(|&s| crate::values::scalar_to_value(s))
            .collect();
        Ok(self.insert(Value::object(shape, slots)))
    }

    // ----- task-local context (native-otel T5a): thin views over `Vm::ctx_current` -----

    fn context_top(&mut self) -> Option<u64> {
        self.vm.ctx_current.last().copied()
    }

    fn context_push(&mut self, v: u64) {
        self.vm.ctx_current.push(v);
    }

    fn context_pop(&mut self, v: u64) {
        if self.vm.ctx_current.last() == Some(&v) {
            self.vm.ctx_current.pop();
        }
    }

    fn context_swap(&mut self, ctx: Vec<u64>) -> Vec<u64> {
        std::mem::replace(&mut self.vm.ctx_current, ctx)
    }

    fn trace_future(&mut self, future: Slot, span: u64) -> CtxResult<bool> {
        let value = self.get(future)?;
        // Only a step future (a lowered `async fn` body) is traceable — both backends draw the
        // same line, so telemetry parity holds for the fallback too.
        if !value.is_step_future() {
            return Ok(false);
        }
        // The table owns one reference; identity = the NaN-box bits, stable while it is held.
        retain(value);
        let mut context = self.vm.ctx_current.clone();
        context.push(span);
        self.vm.traced_futures.push(crate::TracedFuture {
            future: value,
            context,
            span,
        });
        Ok(true)
    }

    fn hot_swap_count(&mut self) -> u64 {
        // Per-VM (server-hmr F5): each worker reports its OWN applied-swap generation, so its
        // serve loop pushes `reload` to its OWN clients when it applies a broadcast swap.
        self.vm.applied_swaps as u64
    }

    fn take_hot_error(&mut self) -> Option<String> {
        self.vm
            .hot_mailbox
            .as_ref()
            .and_then(|m| m.error.lock().ok().and_then(|mut e| e.take()))
    }
}

impl<'m> Vm<'m> {
    /// Call a registered extern type's **higher-order method** (higher-order-abi H4): the
    /// type-method twin of [`Vm::call_ctx_function`] — the receiver rides as slot 0, the
    /// arguments after it.
    pub(crate) fn call_ctx_type_method(
        &mut self,
        type_name: &'static str,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        self.ctx_receiver_call(
            recv,
            args,
            span,
            &format!("{type_name}.{method}"),
            // Compiled-in extensions dispatch through the monomorphized route (every ctx op
            // inlines); anything else falls back to the dyn table (H5 perf).
            |ctx, arg_slots| {
                noeta_stdlib::registry::static_dispatch_ctx_method(
                    type_name, method, ctx, 0, arg_slots,
                )
                .unwrap_or_else(|| {
                    noeta_stdlib::registry::dispatch_ctx_method(
                        type_name, method, ctx, 0, arg_slots,
                    )
                })
            },
            || {
                noeta_stdlib::registry::find_type_ctx_method(type_name, method)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit))
            },
        )
    }

    /// Call a bound method-bundle method (kernel-methods K3): the compiler baked the
    /// `(module, bundle)` route at the call site, so this goes straight to the bundle's shared
    /// ctx dispatch — the bundle twin of [`Vm::call_ctx_type_method`], receiver as slot 0.
    pub(crate) fn call_bundle_method(
        &mut self,
        module: &str,
        bundle: &str,
        method: &str,
        recv: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        self.ctx_receiver_call(
            recv,
            args,
            span,
            &format!("{module}::{bundle}.{method}"),
            |ctx, arg_slots| {
                noeta_stdlib::registry::dispatch_bundle_method(
                    module, bundle, method, ctx, 0, arg_slots,
                )
            },
            || {
                noeta_stdlib::registry::find_bundle(module, bundle)
                    .and_then(|b| b.method(method))
                    .map(|m| m.sig.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit))
            },
        )
    }

    /// The shared receiver-seeded ctx-call shape (extern-type ctx methods + bundle methods):
    /// seed the table directly — receiver as slot 0, arguments after it — index the argument
    /// slots from a stack buffer (a hot `set(v)` loop builds no vectors; the table is pooled),
    /// run `dispatch`, and unwrap the outcome. `ret` resolves the declared `RetTy` lazily — only
    /// a data (`CtxOut::Out`) result pays the lookup.
    fn ctx_receiver_call(
        &mut self,
        recv: Value,
        args: &[Value],
        span: Span,
        label: &str,
        dispatch: impl FnOnce(&mut VmCtx<'_, 'm>, &[Slot]) -> Result<CtxOut, CtxError>,
        ret: impl FnOnce() -> noeta_stdlib::RetTy,
    ) -> Result<Value, Abort> {
        let mut ctx = VmCtx::new(self, &[], span);
        ctx.insert_seed(recv);
        for &a in args {
            ctx.insert_seed(a);
        }
        let mut slot_buf = [0 as Slot; 8];
        let slot_vec;
        let arg_slots: &[Slot] = if args.len() <= 8 {
            for (i, s) in slot_buf.iter_mut().take(args.len()).enumerate() {
                *s = (i + 1) as Slot;
            }
            &slot_buf[..args.len()]
        } else {
            slot_vec = (1..=args.len() as Slot).collect::<Vec<_>>();
            &slot_vec
        };
        let outcome = dispatch(&mut ctx, arg_slots);
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{label}` returned a freed slot"),
                    )),
                }
            }
            // The overwhelmingly common data result of a mutator (`set`) — no shape/ret lookup.
            // A retained arena entry as the result (a `get` over a stable cell): arena load +
            // retain — the arena keeps its reference.
            Ok(CtxOut::Retained(retained)) => {
                drop(ctx);
                match self.ext_arena.get(retained as usize).copied().flatten() {
                    Some(value) => {
                        retain(value);
                        Ok(value)
                    }
                    None => Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        "internal: a native dispatch returned a freed retained handle".to_string(),
                    )),
                }
            }
            Ok(CtxOut::Out(noeta_stdlib::NativeOut::Unit)) => {
                drop(ctx);
                Ok(Value::unit())
            }
            Ok(CtxOut::Out(out)) => {
                drop(ctx);
                let ret = ret();
                // Shape derivation sees the receiver at index 0, as the slots do.
                let mut all = Vec::with_capacity(args.len() + 1);
                all.push(recv);
                all.extend_from_slice(args);
                Ok(materialize_ext(out, ret, &all))
            }
            Err(CtxError::Std(e)) => {
                drop(ctx);
                Err(self.error(stdlib_error_code(e.kind), span, e.message))
            }
            Err(CtxError::Abort) => Err(Abort),
        }
    }

    /// Hot-swap pre-run (server-hmr H1): before a swap fragment that re-runs the top level,
    /// dispose the previous epoch's effects (the re-run re-creates the ones that still exist)
    /// and the reactive nodes held by the globals the fragment is about to re-bind (their
    /// replacements arrive when the fragment runs; preserved subscribers re-subscribe on their
    /// next run). Drives the same shared stdlib disposal code user-level `.dispose()` runs,
    /// through an ephemeral ctx; a program that never touched reactivity no-ops.
    pub(crate) fn hotswap_prepare(&mut self, rebound_slots: &[u32]) {
        let handles: Vec<Value> = rebound_slots
            .iter()
            .filter_map(|&s| self.globals.get(s as usize).copied())
            .filter(|v| !v.is_unbound())
            .collect();
        let span = Span::empty_at_in(noeta_span::SourceId::FIRST, 0);
        let mut ctx = VmCtx::new(self, &handles, span);
        noeta_stdlib::reactive::hotswap_dispose_effects(&mut ctx);
        let slots: Vec<Slot> = (0..handles.len() as Slot).collect();
        noeta_stdlib::reactive::hotswap_dispose_handles(&mut ctx, &slots);
    }

    /// Call a registered **higher-order** native function (higher-order-abi H0): wrap the VM in a
    /// [`VmCtx`], run the shared ctx dispatch, and unwrap the result. The twin of the plain
    /// registry arm in `call_native_module`; the tree-walker mirrors this shape (the dispatch
    /// body itself is shared, so the backends agree by construction).
    pub(crate) fn call_ctx_function(
        &mut self,
        module: &str,
        func: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let mut ctx = VmCtx::new(self, args, span);
        let arg_slots: Vec<Slot> = (0..args.len() as Slot).collect();
        // Compiled-in extensions dispatch through the monomorphized route (H5 perf).
        let outcome =
            noeta_stdlib::registry::static_dispatch_ctx(module, func, &mut ctx, &arg_slots)
                .unwrap_or_else(|| {
                    noeta_stdlib::registry::dispatch_ctx(module, func, &mut ctx, &arg_slots)
                });
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{module}.{func}` returned a freed slot"),
                    )),
                }
            }
            // A retained arena entry as the result (a `get` over a stable cell): arena load +
            // retain — the arena keeps its reference.
            Ok(CtxOut::Retained(retained)) => {
                drop(ctx);
                match self.ext_arena.get(retained as usize).copied().flatten() {
                    Some(value) => {
                        retain(value);
                        Ok(value)
                    }
                    None => Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        "internal: a native dispatch returned a freed retained handle".to_string(),
                    )),
                }
            }
            Ok(CtxOut::Out(out)) => {
                drop(ctx);
                let ret = noeta_stdlib::registry::find_ctx_function(module, func)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit));
                Ok(materialize_ext(out, ret, args))
            }
            Err(CtxError::Std(e)) => {
                drop(ctx);
                Err(self.error(stdlib_error_code(e.kind), span, e.message))
            }
            // The abort's diagnostic is already recorded (the re-entry recorded it); propagate.
            Err(CtxError::Abort) => Err(Abort),
        }
    }
}
