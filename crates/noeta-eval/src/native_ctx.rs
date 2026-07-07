//! The reference interpreter's [`NativeCtx`] implementation (higher-order-abi H0) — the
//! tree-walker twin of the VM's `native_ctx.rs`. Same per-call slot-table shape; here the values
//! are `Rc`-backed clones, so ownership is automatic and the table exists for representation
//! parity (a shared dispatch addresses slots identically in both backends — that is what makes
//! the differential structural).

use noeta_diagnostics::DiagnosticCode;
use noeta_span::Span;
use noeta_stdlib::{
    CtxError, CtxOut, CtxResult, ExternIo, NativeCtx, NativeOut, NativeValue, Slot, StdError,
};

use crate::{
    materialize_ext, materialize_native, std_error_code, value_to_native_deep, Eval, Interpreter,
    Unwind, Value,
};

pub(crate) struct EvalCtx<'i> {
    interp: &'i mut Interpreter,
    /// The slot table (the VM twin's entries own manual references; here a slot owns its clone).
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

impl<'i> EvalCtx<'i> {
    pub(crate) fn new(interp: &'i mut Interpreter, args: &[Value], span: Span) -> EvalCtx<'i> {
        EvalCtx {
            interp,
            slots: args.iter().cloned().map(Some).collect(),
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
                // Ready spends the future slot; the result takes over its index in place (the
                // VM twin's table-reclaim semantics).
                self.slots[future as usize] = Some(result);
                Ok(Some(future))
            }
            Ok(None) => Ok(None),
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
}

impl Interpreter {
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
        let outcome = noeta_stdlib::registry::dispatch_ctx(module, func, &mut ctx, &arg_slots);
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
