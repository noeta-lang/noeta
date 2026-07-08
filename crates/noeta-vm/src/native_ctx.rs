//! The VM's [`NativeCtx`] implementation (higher-order-abi H0): how a registered higher-order
//! native function re-enters the VM. A `VmCtx` is a per-call wrapper over `&mut Vm` plus a
//! **slot table** — each `Some` entry owns exactly one reference to its value (retained on
//! insert, released on [`NativeCtx::free`] or at drop). Centralizing the ownership here is the
//! point: the hand-written `Builtin` arms this seam replaces leaked twice during the http-server
//! arc precisely because every one re-derived the retain/release choreography by hand. A shared
//! dispatch cannot leak a slot — the table's `Drop` releases whatever the dispatch forgot.

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_stdlib::{
    CtxError, CtxOut, CtxResult, ExternIo, NativeCtx, NativeOut, NativeValue, Slot, StdError,
};
use noeta_gc::retain;
use noeta_value::Value;

use crate::values::{materialize_ext, materialize_native};
use crate::{isolate, stdlib_error_code, Abort, Poll, Vm};

pub(crate) struct VmCtx<'c, 'm> {
    vm: &'c mut Vm<'m>,
    /// The slot table: `Some` owns one reference; `None` is freed (index reusable via `free_list`).
    slots: Vec<Option<Value>>,
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

impl<'c, 'm> VmCtx<'c, 'm> {
    /// Wrap the VM for one dispatch, seeding the table with the (borrowed) call arguments —
    /// retained here, so the table's uniform "every entry owns a reference" invariant holds.
    pub(crate) fn new(vm: &'c mut Vm<'m>, args: &[Value], span: Span) -> VmCtx<'c, 'm> {
        let mut slots = Vec::with_capacity(args.len() + 4);
        for &a in args {
            retain(a);
            slots.push(Some(a));
        }
        VmCtx {
            vm,
            slots,
            free_list: Vec::new(),
            span,
        }
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
    /// one reference transfers to the caller).
    pub(crate) fn take(&mut self, slot: Slot) -> CtxResult<Value> {
        self.slots
            .get_mut(slot as usize)
            .and_then(Option::take)
            .ok_or_else(bad_slot)
    }
}

impl Drop for VmCtx<'_, '_> {
    fn drop(&mut self) {
        // Release every reference the dispatch left behind (its arguments, forgotten temps).
        for value in std::mem::take(&mut self.slots).into_iter().flatten() {
            self.vm.release_value(value);
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
        let element = self
            .get(list)?
            .list_get(index)
            .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("call_with_element", "list")))?;
        // The element rides straight into the callee's frame (which consumes the reference) —
        // no table entry, exactly the fused `list_get` + `call` + `free`.
        retain(element);
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
        let element = self
            .get(list)?
            .list_get(index)
            .ok_or_else(|| CtxError::Std(noeta_stdlib::type_error("list_get", "list")))?;
        // `list_get` hands out a borrowed reference; the table owns its entries.
        retain(element);
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
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend_from_slice(args);
        let mut ctx = VmCtx::new(self, &all, span);
        let arg_slots: Vec<Slot> = (1..all.len() as Slot).collect();
        let outcome = noeta_stdlib::registry::dispatch_ctx_method(
            type_name, method, &mut ctx, 0, &arg_slots,
        );
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{type_name}.{method}` returned a freed slot"),
                    )),
                }
            }
            Ok(CtxOut::Out(out)) => {
                drop(ctx);
                let ret = noeta_stdlib::registry::find_type_ctx_method(type_name, method)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit));
                // Shape derivation sees the same `[recv, args…]` view the slots use.
                Ok(materialize_ext(out, ret, &all))
            }
            Err(CtxError::Std(e)) => {
                drop(ctx);
                Err(self.error(stdlib_error_code(e.kind), span, e.message))
            }
            Err(CtxError::Abort) => Err(Abort),
        }
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
        let outcome = noeta_stdlib::registry::dispatch_ctx(module, func, &mut ctx, &arg_slots);
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
