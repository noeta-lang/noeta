//! The reference interpreter's [`NativeCtx`] implementation (higher-order-abi H0) — the
//! tree-walker twin of the VM's `native_ctx.rs`. Same per-call slot-table shape; here the values
//! are `Rc`-backed clones, so ownership is automatic and the table exists for representation
//! parity (a shared dispatch addresses slots identically in both backends — that is what makes
//! the differential structural).

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_stdlib::{
    CtxError, CtxOut, CtxResult, ExternIo, NativeCtx, NativeOut, NativeValue, Retained, Slot,
    StdError,
};

use crate::{
    materialize_ext, materialize_native, std_error_code, value_to_native_deep, Eval, Interpreter,
    Unwind, Value,
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
        let id = self
            .interp
            .executor
            .spawn_ext(&mut *self.interp.host, io);
        self.insert(Value::AsyncIo(id))
    }

    fn timer(&mut self, ms: u64) -> Slot {
        let deadline = self.interp.executor.now() + ms;
        self.insert(Value::Timer(deadline))
    }

    fn poll(&mut self, future: Slot) -> CtxResult<Option<Slot>> {
        let value = self.get(future)?.clone();
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
}

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
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(recv);
        all.extend_from_slice(args);
        let mut ctx = EvalCtx::new(self, &all, span);
        let arg_slots: Vec<Slot> = (1..all.len() as Slot).collect();
        // Same monomorphized route as the VM (identical fn either way — the instantiation only
        // decides inlining).
        let outcome = noeta_stdlib::registry::static_dispatch_ctx_method(
            type_name, method, &mut ctx, 0, &arg_slots,
        )
        .unwrap_or_else(|| {
            noeta_stdlib::registry::dispatch_ctx_method(type_name, method, &mut ctx, 0, &arg_slots)
        });
        match outcome {
            Ok(CtxOut::Slot(slot)) => {
                let result = ctx.take(slot);
                drop(ctx);
                match result {
                    Ok(value) => Ok(value),
                    Err(_) => Err(self.runtime_error(
                        DiagnosticCode::Panic,
                        span,
                        format!("internal: `{type_name}.{method}` returned a freed slot"),
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
                let ret = noeta_stdlib::registry::find_type_ctx_method(type_name, method)
                    .map(|f| f.ret)
                    .unwrap_or(noeta_stdlib::RetTy::Concrete(noeta_stdlib::SigType::Unit));
                Ok(materialize_ext(out, ret, &all))
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
        let mut ctx = EvalCtx::new(self, args, span);
        let arg_slots: Vec<Slot> = (0..args.len() as Slot).collect();
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
                let ret = noeta_stdlib::registry::find_ctx_function(module, func)
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
