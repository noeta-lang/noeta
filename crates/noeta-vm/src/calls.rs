//! The **re-entrant call layer**: [`Vm::call_value`] (native → user-code
//! re-entry on a fresh frame stack), `run_method_handle` / `run_thunk`,
//! `setup_closure_call`, the `do_return*` pair, `call_native_fn`,
//! `call_builtin`, and `check_arity`. Every item is an `impl Vm` method moved
//! verbatim from the crate root purely to shrink `lib.rs` — no behavior
//! change (`#[inline]` attributes preserved).

use crate::*;

impl<'m> Vm<'m> {
    /// Pop a pooled re-entrant run context (audit-1 finding 5): an empty frame stack plus a
    /// register stack sized to `num_registers` (unit-filled). The caller fills the argument
    /// registers, pushes its entry frame, and hands both to [`Vm::run`], which clears and
    /// returns them to the pool on exit — so a per-element re-entry (`xs.map(f)`) allocates
    /// nothing once the pool is warm.
    #[inline]
    pub(crate) fn pooled_run_stacks(&mut self, num_registers: usize) -> (Vec<Frame>, Vec<Value>) {
        let (frames, mut regs) = self.reentry_pool.pop().unwrap_or_default();
        debug_assert!(
            frames.is_empty() && regs.is_empty(),
            "pooled stacks are clean"
        );
        regs.resize(num_registers, Value::unit());
        (frames, regs)
    }

    /// Resolve `type_name.method` in the instance-method table with **borrowed** keys
    /// (audit-1 finding 7): two `&str` probes of the two-level map replace the flat
    /// `(String, String)`-key lookup that heap-allocated both names per dynamic dispatch
    /// (enum methods, operator overloads, `Op::Invoke`, and every cache miss).
    #[inline]
    pub(crate) fn method_proto(&self, type_name: &str, method: &str) -> Option<u32> {
        self.methods
            .get(type_name)
            .and_then(|methods| methods.get(method))
            .copied()
    }

    /// Whether a value is a user object exposing a `next` member — a declared method, or a field
    /// slot (whose value the drain calls through the ordinary indirect-call path; a non-callable
    /// one raises its error there). The gate for `next`-driven user iteration, mirroring the
    /// tree-walker's.
    pub(crate) fn has_user_next(&self, v: Value) -> bool {
        v.is_object()
            && v.shape().is_some_and(|s| {
                self.method_proto(&s.name, "next").is_some() || s.slot_of("next").is_some()
            })
    }

    /// Drain a `next`-driven user iterator object into a materialized element list — the
    /// member-handle iterator (coroutines Track-I trigger): call the object's `next` member — a
    /// method (the same synchronous re-entry a bound handle uses), or a closure-valued field
    /// (called like any function value) — until it returns `none`; each `some(x)` contributes
    /// `x`. Eager like the `Iterable` list path (user iteration snapshots; lazy streaming remains
    /// built-in `Iterator<T>`'s). A step that is not a built-in option is E0007, identically in
    /// both backends. `obj` is borrowed (the caller's register keeps it alive); the returned list
    /// owns one reference per element.
    pub(crate) fn drain_next_object(&mut self, obj: Value, span: Span) -> Result<Value, Abort> {
        let mut elements: Vec<Value> = Vec::new();
        loop {
            let stepped = {
                let shape = obj.shape().expect("a user iterator object has a shape");
                if self.method_proto(&shape.name, "next").is_some() {
                    retain(obj);
                    self.run_method_handle("", "next", false, vec![obj], span)
                } else {
                    let f = shape
                        .slot_of("next")
                        .and_then(|s| obj.slot_at(s))
                        .expect("gated on a `next` member");
                    // Retained across the call: the closure body may reassign the very field
                    // it was read from (a `class` field-set mutates in place), so the slot's
                    // reference alone cannot be relied on for the call's duration.
                    retain(f);
                    let result = self.call_value(f, Vec::new(), span);
                    release(f);
                    result
                }
            };
            let step = match stepped {
                Ok(v) => v,
                Err(abort) => {
                    for e in elements {
                        release(e);
                    }
                    return Err(abort);
                }
            };
            let variant = step
                .shape()
                .filter(|s| s.name == "Option")
                .and_then(|s| s.variant.clone());
            match variant.as_deref() {
                Some("some") => {
                    let payload = step
                        .enum_data()
                        .and_then(|d| d.into_iter().next())
                        .unwrap_or_else(Value::unit);
                    retain(payload);
                    release(step);
                    elements.push(payload);
                }
                Some("none") => {
                    release(step);
                    return Ok(Value::list(elements));
                }
                _ => {
                    let found = step.type_name();
                    release(step);
                    for e in elements {
                        release(e);
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("iterator `next` must return an option, found {found}"),
                    ));
                }
            }
        }
    }

    /// Call a value with already-owned arguments (each carrying one reference transferred to
    /// the callee), re-entering the VM on a fresh frame stack. Only closures are callable in
    /// this slice — builtins are never first-class values. Used by `map`/`filter`.
    pub(crate) fn call_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, Abort> {
        match callee.as_closure() {
            Some(proto) => {
                let chunk = &self.module.protos[proto as usize];
                let num_params = chunk.num_params as usize;
                let num_registers = chunk.num_registers as usize;
                let required = num_params - chunk.defaults.len();
                let defaults = chunk.defaults.clone();
                if args.len() < required || args.len() > num_params {
                    let supplied = args.len();
                    for a in args {
                        release(a);
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        arity_message("function", required, num_params, supplied),
                    ));
                }
                let filled = args.len();
                let (mut frames, mut regs) = self.pooled_run_stacks(num_registers);
                for (i, v) in args.into_iter().enumerate() {
                    regs[i] = v;
                }
                // A first-class closure may capture upvalues; carry its cells into the re-entrant
                // frame (one owned reference each) and hand them to each default thunk, which shares
                // the closure's upvalue layout so a capture-referencing default reads the right cell.
                let count = callee.closure_upvalue_count();
                let cells: Vec<Value> = (0..count).map(|i| callee.closure_upvalue(i)).collect();
                // Fill any omitted trailing parameters from their default thunks.
                for (reg, dproto) in &defaults {
                    if *reg as usize >= filled {
                        let value = self.run_thunk(*dproto, &cells)?;
                        regs[*reg as usize] = value;
                    }
                }
                let mut upvalues = Vec::with_capacity(count);
                for &cell in &cells {
                    retain(cell);
                    upvalues.push(cell);
                }
                frames.push(Frame {
                    proto,
                    base: 0,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                    upvalues,
                });
                self.run(frames, regs)
            }
            None => match callee.as_native_fn() {
                // A first-class builtin passed as the callee (e.g. `map(xs, len)`). The args are
                // owned here, so release them after the borrowing helper returns.
                Some(func) => {
                    let result = self.call_native_fn(func, &args, span);
                    for a in &args {
                        release(*a);
                    }
                    result
                }
                // A selectively-imported native-module function (`use std.math.sqrt`) called by its
                // bare name — dispatched through the same `call_native_module` as `math.sqrt(...)`.
                None => match callee.module_fn_parts() {
                    Some((module, func)) => {
                        let result = self.call_native_module(&module, &func, &args, span);
                        for a in &args {
                            release(*a);
                        }
                        result
                    }
                    // An unbound method handle (`Type.method`) applied to its arguments — the first is
                    // the receiver (prelude-redesign MH). Runs the resolved method on a fresh frame
                    // stack, consuming the owned arguments into the callee window.
                    None => match callee.method_handle_parts() {
                        Some((ty, method, associated)) => {
                            self.run_method_handle(&ty, &method, associated, args, span)
                        }
                        // A bound handle: prepend the captured receiver (retained — the instance
                        // dispatch consumes owned arguments) and run as an instance handle.
                        None => match callee.bound_method_parts() {
                            Some((recv, method)) => {
                                retain(recv);
                                let mut owned = Vec::with_capacity(args.len() + 1);
                                owned.push(recv);
                                owned.extend(args);
                                self.run_method_handle("", &method, false, owned, span)
                            }
                            None => {
                                let type_name = callee.type_name();
                                for a in args {
                                    release(a);
                                }
                                Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    span,
                                    format!("{type_name} is not callable"),
                                ))
                            }
                        },
                    },
                },
            },
        }
    }

    /// Dispatch a **built-in** method on a non-object receiver — every method-call path that is
    /// value-in/value-out: `compare`, string/int/numeric-conversion methods, the Ring 1
    /// list/set/map/iterator/file-handle methods, channel endpoints, reactive handles, `to_bytes`,
    /// `iter()`, the eager `map`/`filter`/`sum`, and `count`/`len`/`enumerate` — ending in the
    /// canonical takes-no-arguments / no-method errors. Factored out of the `Op::CallMethod` arm
    /// (prelude-redesign MH.2) so the opcode and an unbound method handle (`list.len` passed as a
    /// value) dispatch through the SAME branches by construction. The op-field-dependent fast paths
    /// (`reuse` in-place updates) and the frame-pushing object/enum dispatches stay in the opcode —
    /// receivers here never resolve through the user method table.
    ///
    /// The receiver and arguments are **borrowed** (the caller keeps its references, exactly as the
    /// opcode's registers did); the result is a freshly-owned value. Branch ORDER is semantic
    /// (string before int, `IntMethod` before `NumConvert`, …) — do not reorder.
    ///
    /// `#[inline]` so the `Op::CallMethod` arm — the hot call site — folds this back into the
    /// dispatch loop exactly as the pre-extraction inline branches were (A/B-benched: the bare
    /// out-of-line call cost ~+15-25ns per built-in method call); the cold handle path may call it
    /// out-of-line. `hk` is the receiver's one-shot [`HeapKind`] classification (the caller already
    /// derefs it once), so every rung below is an integer compare — main's classify-once dispatch,
    /// preserved through the extraction.
    #[inline]
    pub(crate) fn call_builtin_method(
        &mut self,
        v: Value,
        hk: Option<HeapKind>,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // `x.compare(y)` — the `Ordering` of two primitives (the value a `Comparable`
        // impl returns). One argument, on any non-object receiver.
        if method == "compare" {
            if args.len() != 1 {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "method `compare` takes 1 argument but {} were supplied",
                        args.len()
                    ),
                ));
            }
            let other = args[0];
            return match compare_primitive(v, other) {
                Some(ordering) => Ok(make_ordering(noeta_ast::ordering_variant(ordering))),
                None => Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("cannot compare {} and {}", v.type_name(), other.type_name()),
                )),
            };
        }
        // Ring 1 string methods (`upper`/`split`/`replace`/...) — dispatched through
        // the shared `noeta-stdlib` surface so the tree-walker and the VM cannot drift.
        // `Unknown` falls through to the collection methods below. `as_string` clones
        // out of the heap, so the projected args own their strings for the call.
        if hk == Some(HeapKind::Str)
            && let Some(recv_str) = v.as_string()
        {
            let arg_strings: Vec<Option<String>> = args.iter().map(|a| a.as_string()).collect();
            let projected: Vec<noeta_stdlib::Arg> = args
                .iter()
                .zip(&arg_strings)
                .map(|(a, s)| {
                    if let Some(s) = s {
                        noeta_stdlib::Arg::Str(s)
                    } else if let Some(i) = a.as_int() {
                        noeta_stdlib::Arg::Int(i)
                    } else if let Some(f) = a.as_float() {
                        noeta_stdlib::Arg::Float(f)
                    } else if let Some(b) = a.as_bool() {
                        noeta_stdlib::Arg::Bool(b)
                    } else {
                        noeta_stdlib::Arg::Other
                    }
                })
                .collect();
            match noeta_stdlib::string_method(&recv_str, method, &projected) {
                noeta_stdlib::Dispatch::Done(output) => {
                    return Ok(stdlib_output_to_value(output));
                }
                noeta_stdlib::Dispatch::Err(error) => {
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                noeta_stdlib::Dispatch::Unknown => {}
            }
        }
        // Bit-manipulation methods on `int` (P-BITS Tier B4) — the popcount-class
        // intrinsics, delegating to the shared `int_method` so the backends agree. The
        // checker already arity/type-checked the call; `rotate_*` take one `int` amount.
        if matches!(hk, None | Some(HeapKind::Int))
            && let Some(recv_int) = v.as_int()
            && let Some(int_method) = noeta_stdlib::IntMethod::from_name(method)
        {
            let arg = match args.first() {
                Some(a) => match a.as_int() {
                    Some(n) => n,
                    None => {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!(
                                "`int.{method}` expects an integer argument, found {}",
                                a.type_name()
                            ),
                        ));
                    }
                },
                None => 0,
            };
            return Ok(Value::int(noeta_stdlib::int_method(
                recv_int, int_method, arg,
            )));
        }
        // Cross-domain numeric conversions (S0): `int→float/f32`, `float/f32→int`,
        // `float↔f32`. The `IntMethod` branch above handled `int→int` and returned; an
        // integer receiver reaches here only for a float destination (`to_float`/`to_f32`),
        // a `float`/`f32` receiver for any. Shared `num_convert` keeps the backends in step.
        if matches!(hk, None | Some(HeapKind::Int))
            && let Some(src) = v
                .as_f32()
                .map(noeta_stdlib::NumScalar::F32)
                .or_else(|| v.as_float().map(noeta_stdlib::NumScalar::F64))
                .or_else(|| v.as_int().map(noeta_stdlib::NumScalar::Int))
            && let Some(dest) = noeta_stdlib::NumConvert::from_name(method)
        {
            return Ok(match noeta_stdlib::num_convert(src, dest) {
                noeta_stdlib::NumScalar::Int(i) => Value::int(i),
                noeta_stdlib::NumScalar::F64(f) => Value::float(f),
                noeta_stdlib::NumScalar::F32(f) => Value::f32(f),
            });
        }
        // Ring 1 list methods (reverse/contains/join) — the shared `ListMethod` enum
        // makes the helper's `match` exhaustive, so the tree-walker cannot offer a
        // method this backend lacks.
        if matches!(hk, Some(HeapKind::List | HeapKind::PackedList))
            && let Some(list_method) = noeta_stdlib::ListMethod::from_name(method)
        {
            return self.call_list_method(v, list_method, method, args, span);
        }
        // Ring 1 set methods (contains/union/intersection).
        if hk == Some(HeapKind::Set)
            && let Some(set_method) = noeta_stdlib::SetMethod::from_name(method)
        {
            return self.call_set_method(v, set_method, method, args, span);
        }
        // Extern-type methods (extern-types X1): every registry-contributed type routes through
        // its registered `ExtType`'s one shared dispatch.
        if hk == Some(HeapKind::Extern) {
            return self.call_extern_method(v, method, args, span);
        }
        // Channel endpoint methods (isolates I.1): `tx.send(v)`/`tx.close()` on a sender,
        // `rx.recv()` on a receiver. `send`/`recv` yield leaf futures (enqueue/dequeue when
        // polled); `close` is synchronous. Endpoint validity was checked statically.
        if hk == Some(HeapKind::Sender)
            && let Some(id) = v.sender_id()
        {
            match method {
                "send" => {
                    // The future retains its own reference to the message; the caller's
                    // reference is released by its normal end-of-life.
                    return Ok(Value::make_channel_send(id, args[0]));
                }
                "close" => {
                    match &mut self.persist.channels[id.index()] {
                        Channel::Local { closed, .. } => *closed = true,
                        Channel::Shared(core) => core.close(),
                    }
                    self.persist.channel_progress += 1;
                    return Ok(Value::unit());
                }
                _ => {}
            }
        }
        if hk == Some(HeapKind::Receiver)
            && let Some(id) = v.receiver_id()
            && method == "recv"
        {
            return Ok(Value::make_channel_recv(id));
        }
        // (The reactive handle methods lived here until higher-order-abi H5 — `Signal`/
        // `Computed`/`Effect` are registry extern types now, dispatched through the ctx
        // seam like any other; `get` inlines via the declared arena read.)
        // Iterator methods (next/collect) — the shared `IterMethod` enum, like the file
        // handle above.
        if hk == Some(HeapKind::Iter)
            && let Some(iter_method) = noeta_stdlib::IterMethod::from_name(method)
        {
            return self.call_iter_method(v, iter_method, method, args, span);
        }
        // Ring 1 map methods (keys/values/has).
        if hk == Some(HeapKind::Map)
            && let Some(map_method) = noeta_stdlib::MapMethod::from_name(method)
        {
            return self.call_map_method(v, map_method, method, args, span);
        }
        // `list.to_bytes()` — serialize a `List<@packed>` to its flat buffer (P-PACK 4.4);
        // a boxed list has no canonical form, so it's a type error (surfaced, not silent).
        if method == "to_bytes" && matches!(hk, Some(HeapKind::List | HeapKind::PackedList)) {
            if !args.is_empty() {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "method `to_bytes` takes no arguments".to_string(),
                ));
            }
            return match v.packed_bytes() {
                Some(buf) => Ok(Value::bytes(buf)),
                None => Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "`to_bytes` expects a packed list (a `List` of `@packed` structs)".to_string(),
                )),
            };
        }
        // `iter()` on a built-in collection (Track I.1a) → a lazy iterator. A list shares
        // its backing (the iterator retains one reference); a set/map first becomes a list
        // of its elements / values (the iteration order `for` uses).
        if method == "iter"
            && matches!(
                hk,
                Some(HeapKind::List | HeapKind::PackedList | HeapKind::Set | HeapKind::Map)
            )
        {
            if !args.is_empty() {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "method `iter` takes no arguments".to_string(),
                ));
            }
            let value = if matches!(hk, Some(HeapKind::List | HeapKind::PackedList)) {
                Value::iter(v)
            } else {
                let items = if hk == Some(HeapKind::Set) {
                    v.set_items()
                } else {
                    v.map_values()
                }
                .expect("set/map receiver");
                for item in &items {
                    item.inc_ref();
                }
                let list = Value::list(items);
                let iter = Value::iter(list);
                // `Value::iter` retained the list; drop this local reference so the
                // iterator is its sole owner.
                list.release();
                iter
            };
            return Ok(value);
        }
        // Eager collection methods reusing the prelude builtin impls (prelude-redesign
        // P1): `xs.map(f)` / `xs.filter(f)` / `xs.sum()` on a list, routed through
        // `call_builtin` with the receiver as the first argument so the method and
        // (legacy) free-function forms share one impl. A user object's own method wins
        // (dispatched earlier); a list receiver is never an object.
        if matches!(hk, Some(HeapKind::List | HeapKind::PackedList))
            && let Some(builtin) = match method {
                "map" if args.len() == 1 => Some(Builtin::Map),
                "filter" if args.len() == 1 => Some(Builtin::Filter),
                "sum" if args.is_empty() => Some(Builtin::Sum),
                _ => None,
            }
        {
            let mut arg_vals = Vec::with_capacity(args.len() + 1);
            arg_vals.push(v);
            arg_vals.extend_from_slice(args);
            return self.call_builtin(builtin, &arg_vals, span);
        }
        // Built-in zero-argument methods on lists/maps/strings. `len()` is the collection
        // length (P1.3 — `count` is iterator-only, a consuming terminal).
        let result = if !args.is_empty() {
            None
        } else if method == "len" {
            v.list_len()
                .or_else(|| v.set_len())
                .or_else(|| v.map_len())
                .or_else(|| v.as_string().map(|s| s.chars().count()))
                .or_else(|| v.bytes_len())
                .map(|n| Value::int(n as i64))
        } else if method == "to_hex" {
            // Lowercase hex rendering of a `bytes` buffer (crypto arc C1) — the shared helper,
            // so both backends print digests identically.
            v.bytes_data()
                .map(|b| Value::string(&noeta_stdlib::bytes_to_hex(&b)))
        } else if method == "decode" {
            // UTF-8 decode — the inverse of `string.to_bytes()`; invalid UTF-8 is `none`.
            v.bytes_data()
                .map(|b| match noeta_stdlib::bytes_decode_utf8(&b) {
                    Some(s) => make_some(Value::string(&s)),
                    None => make_none(),
                })
        } else if method == "enumerate" && matches!(hk, Some(HeapKind::List | HeapKind::PackedList))
        {
            // A list of `(index, value)` **tuples** (object-model slice 4b), matching the
            // tree-walker's `Value::Tuple` pairs. A packed list is materialized to a
            // temporary boxed list first (then released).
            let boxed = v.realize_list();
            let items = boxed.list_items().expect("list receiver");
            let pairs = items
                .iter()
                .enumerate()
                .map(|(i, &element)| {
                    retain(element);
                    Value::tuple(vec![Value::int(i as i64), element])
                })
                .collect();
            boxed.release();
            Some(Value::list(pairs))
        } else {
            None
        };
        match result {
            Some(value) => Ok(value),
            None if !args.is_empty() && (method == "len" || method == "enumerate") => Err(self
                .error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("method `{method}` takes no arguments"),
                )),
            None => Err(self.error(
                DiagnosticCode::UnknownName,
                span,
                format!("no method `{method}` on {}", v.type_name()),
            )),
        }
    }

    /// Run an unbound method handle (`Type.method`) applied to `args` on a fresh frame stack,
    /// consuming the owned arguments into the callee window (prelude-redesign MH). For an **instance**
    /// handle the first argument is the receiver (register 0 = `self`), the rest are the method's
    /// parameters — identical to a closure call whose prototype is resolved from the method table
    /// rather than a first-class closure. Associated handles are not yet produced (MH.1 is
    /// instance-only); they return a clean error rather than mis-dispatching.
    fn run_method_handle(
        &mut self,
        ty: &str,
        method: &str,
        associated: bool,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, Abort> {
        // An ASSOCIATED handle (`ctor = Stack.new`, prelude-redesign EX.2) calls the function
        // directly — no receiver; the prototype's register 0 (`self`) stays unit, exactly as the
        // opcode's associated dispatch leaves it.
        if associated {
            let Some(proto) = self.method_proto(ty, method) else {
                for a in args {
                    release(a);
                }
                return Err(self.error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{ty}` has no associated function `{method}`"),
                ));
            };
            let chunk = &self.module.protos[proto as usize];
            // Register 0 is the (unit) receiver slot, so declared arity is one more than the args.
            let total = chunk.num_params as usize - 1;
            let required = total - chunk.defaults.len();
            if args.len() < required || args.len() > total {
                let supplied = args.len();
                for a in args {
                    release(a);
                }
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    arity_message("associated function", required, total, supplied),
                ));
            }
            let filled = args.len() + 1;
            let num_registers = chunk.num_registers as usize;
            let defaults = chunk.defaults.clone();
            let (mut frames, mut regs) = self.pooled_run_stacks(num_registers);
            for (i, v) in args.into_iter().enumerate() {
                regs[i + 1] = v;
            }
            for (reg, dproto) in &defaults {
                if *reg as usize >= filled {
                    let value = self.run_thunk(*dproto, &[])?;
                    regs[*reg as usize] = value;
                }
            }
            frames.push(Frame {
                proto,
                base: 0,
                pc: 0,
                ret_dst: 0,
                ret_transform: RetTransform::None,
                upvalues: Vec::new(),
            });
            return self.run(frames, regs);
        }
        // The receiver's runtime type names the method table entry, so a subtype dispatches to its
        // own method; fall back to the handle's declared type if the receiver has no shape.
        let type_name = match args.first() {
            Some(recv) => recv
                .shape()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| ty.to_string()),
            None => {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("method handle `{ty}.{method}` needs a receiver argument"),
                ));
            }
        };
        let Some(proto) = self.method_proto(&type_name, method) else {
            // Not a user method — a **built-in** receiver (`list.len`, `string.upper`, MH.2):
            // dispatch through the same `call_builtin_method` the `Op::CallMethod` opcode uses, so a
            // handle call and a direct call agree by construction (this mirrors the tree-walker,
            // whose handle arm reuses its ordinary `call_method`). The helper borrows; the owned
            // arguments are released after (the result is a fresh value, so this is safe even when
            // it aliases an argument's content).
            let recv = args[0];
            let result = self.call_builtin_method(recv, recv.heap_kind(), method, &args[1..], span);
            for a in args {
                release(a);
            }
            return result;
        };
        let chunk = &self.module.protos[proto as usize];
        let num_params = chunk.num_params as usize; // includes register 0 = self (the receiver)
        let num_registers = chunk.num_registers as usize;
        let required = num_params - chunk.defaults.len();
        if args.len() < required || args.len() > num_params {
            let supplied = args.len();
            for a in args {
                release(a);
            }
            return Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, num_params, supplied),
            ));
        }
        let filled = args.len();
        let defaults = chunk.defaults.clone();
        let (mut frames, mut regs) = self.pooled_run_stacks(num_registers);
        for (i, v) in args.into_iter().enumerate() {
            regs[i] = v;
        }
        // A method never captures upvalues; fill any omitted trailing defaults from module scope.
        for (reg, dproto) in &defaults {
            if *reg as usize >= filled {
                let value = self.run_thunk(*dproto, &[])?;
                regs[*reg as usize] = value;
            }
        }
        frames.push(Frame {
            proto,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        });
        self.run(frames, regs)
    }

    /// Run a defaulted parameter's zero-argument thunk prototype to its value, on a fresh frame
    /// stack (the same re-entry `map`/`filter` callbacks use). `upvalues` are the calling closure's
    /// captured cells — the thunk is compiled with that same upvalue layout, so a default that
    /// references a captured variable reads the right cell; for a top-level function or method this
    /// is empty and the thunk resolves globals only. Each cell is retained for the thunk frame (and
    /// released at its teardown). The returned value owns one reference, transferred to its register.
    pub(crate) fn run_thunk(&mut self, proto: u32, upvalues: &[Value]) -> Result<Value, Abort> {
        let num_registers = self.module.protos[proto as usize].num_registers as usize;
        let mut ups = Vec::with_capacity(upvalues.len());
        for &cell in upvalues {
            retain(cell);
            ups.push(cell);
        }
        let (mut frames, regs) = self.pooled_run_stacks(num_registers);
        frames.push(Frame {
            proto,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: ups,
        });
        self.run(frames, regs)
    }

    /// Set up a call to `callee_val` on the shared frame/register stacks — the closure-call machinery
    /// shared by the `Op::Call` interpreter arm and the JIT's `jit_call` helper (so it lives in one
    /// place). Reads the arguments from `regs[caller_base + arg_regs[i]]`, moves them into a fresh
    /// callee window, fills defaults, carries upvalues, saves `resume_pc` on the caller frame, and
    /// pushes the callee frame. Returns `Ok(true)` when a frame was pushed (the caller should re-derive
    /// its window — `continue 'reload`), or `Ok(false)` when the call completed synchronously (a
    /// first-class builtin, result already in `regs[caller_base + dst]`; the caller advances to
    /// `resume_pc`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn setup_closure_call(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        caller_top: usize,
        caller_base: usize,
        dst: u16,
        callee_val: Value,
        arg_regs: &[u16],
        span: Span,
        resume_pc: usize,
    ) -> Result<bool, Abort> {
        match callee_val.as_closure() {
            Some(proto_idx) => {
                let callee_chunk = &self.module.protos[proto_idx as usize];
                let num_params = callee_chunk.num_params as usize;
                let required = num_params - callee_chunk.defaults.len();
                if arg_regs.len() < required || arg_regs.len() > num_params {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        arity_message("function", required, num_params, arg_regs.len()),
                    ));
                }
                let num_registers = callee_chunk.num_registers as usize;
                let new_base = reserve_window(regs, num_registers);
                for (i, &arg_reg) in arg_regs.iter().enumerate() {
                    let v = regs[caller_base + arg_reg as usize];
                    retain(v);
                    regs[new_base + i] = v;
                }
                let count = callee_val.closure_upvalue_count();
                // Fast path (B): a plain function — no defaults to fill and no upvalues to carry —
                // skips the defaults clone, the cell collection, the default-thunk loop, and the
                // upvalue vector entirely. This is the shape of every top-level `fn` call.
                let upvalues = if callee_chunk.defaults.is_empty() && count == 0 {
                    Vec::new()
                } else {
                    let defaults = callee_chunk.defaults.clone();
                    let cells: Vec<Value> =
                        (0..count).map(|i| callee_val.closure_upvalue(i)).collect();
                    let filled = arg_regs.len();
                    for (reg, proto) in &defaults {
                        if *reg as usize >= filled {
                            let value = self.run_thunk(*proto, &cells)?;
                            regs[new_base + *reg as usize] = value;
                        }
                    }
                    let mut upvalues = Vec::with_capacity(count);
                    for &cell in &cells {
                        retain(cell);
                        upvalues.push(cell);
                    }
                    upvalues
                };
                frames[caller_top].pc = resume_pc;
                frames.push(Frame {
                    proto: proto_idx,
                    base: new_base,
                    pc: 0,
                    ret_dst: dst,
                    ret_transform: RetTransform::None,
                    upvalues,
                });
                Ok(true)
            }
            None => match callee_val.as_native_fn() {
                Some(func) => {
                    let arg_vals: Vec<Value> = arg_regs
                        .iter()
                        .map(|&r| regs[caller_base + r as usize])
                        .collect();
                    let result = self.call_native_fn(func, &arg_vals, span)?;
                    set_reg(regs, caller_base, dst, result);
                    Ok(false)
                }
                // A selectively-imported native-module function called by its bare name.
                None => match callee_val.module_fn_parts() {
                    Some((module, func)) => {
                        let arg_vals: Vec<Value> = arg_regs
                            .iter()
                            .map(|&r| regs[caller_base + r as usize])
                            .collect();
                        let result = self.call_native_module(&module, &func, &arg_vals, span)?;
                        set_reg(regs, caller_base, dst, result);
                        Ok(false)
                    }
                    // An unbound method handle (`Type.method`) stored and called directly. Run it
                    // synchronously (its method body re-enters the VM) and land the result — the
                    // arguments are retained since `run_method_handle` consumes owned references.
                    None => match callee_val.method_handle_parts() {
                        Some((ty, method, associated)) => {
                            let arg_vals: Vec<Value> = arg_regs
                                .iter()
                                .map(|&r| {
                                    let v = regs[caller_base + r as usize];
                                    retain(v);
                                    v
                                })
                                .collect();
                            let result =
                                self.run_method_handle(&ty, &method, associated, arg_vals, span)?;
                            set_reg(regs, caller_base, dst, result);
                            Ok(false)
                        }
                        // A bound handle: captured receiver first, then the call's arguments.
                        None => match callee_val.bound_method_parts() {
                            Some((recv, method)) => {
                                retain(recv);
                                let mut owned = Vec::with_capacity(arg_regs.len() + 1);
                                owned.push(recv);
                                for &r in arg_regs {
                                    let v = regs[caller_base + r as usize];
                                    retain(v);
                                    owned.push(v);
                                }
                                let result =
                                    self.run_method_handle("", &method, false, owned, span)?;
                                set_reg(regs, caller_base, dst, result);
                                Ok(false)
                            }
                            None => {
                                // The **`Callable` protocol**: an object (or enum value) invoked
                                // as a value — `obj(args)` dispatches to its `call` METHOD
                                // (receiver first, then the call's arguments) through the same
                                // synchronous re-entry a bound handle uses. Structural at runtime
                                // like the other protocol dispatches (`iter`, `to_string`): the
                                // method table is consulted, `impl Callable { fn call(...) }` the
                                // validated way to populate it. Method-only, matching the
                                // tree-walker's gate — a closure-valued FIELD named `call` is
                                // member-call territory (`obj.call(args)`), not invocability.
                                if let Some(shape) = callee_val.shape()
                                    && self.method_proto(&shape.name, "call").is_some()
                                {
                                    retain(callee_val);
                                    let mut owned = Vec::with_capacity(arg_regs.len() + 1);
                                    owned.push(callee_val);
                                    for &r in arg_regs {
                                        let v = regs[caller_base + r as usize];
                                        retain(v);
                                        owned.push(v);
                                    }
                                    let result =
                                        self.run_method_handle("", "call", false, owned, span)?;
                                    set_reg(regs, caller_base, dst, result);
                                    return Ok(false);
                                }
                                Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    span,
                                    format!("{} is not callable", callee_val.type_name()),
                                ))
                            }
                        },
                    },
                },
            },
        }
    }

    /// The `Op::Return` protocol, factored so both the interpreter arm and the JIT's `jit_return`
    /// helper share it (J3 native calls). `raw` is the value being returned (already read from the
    /// returning frame). Retains it across teardown, pops the finished frame, releases its register
    /// window and upvalues, truncates the register stack, applies any `ret_transform`, and transfers
    /// the result into the caller's destination register. Returns `Some(v)` when the **bottom** frame
    /// returned (there is no caller — `run` should yield `v`), or `None` when it transferred to a
    /// caller (control resumes in that caller frame).
    pub(crate) fn do_return(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        raw: Value,
    ) -> Option<Value> {
        self.do_return_masked(frames, regs, raw, u64::MAX)
    }

    /// [`Vm::do_return`] with a window-release mask (P-JSSA S4.0, see [`jit_return`]):
    /// `u64::MAX` releases every slot (the interpreter's path — it has no per-site analysis);
    /// any other value releases only the set bits, a guarantee from the JIT that the clear
    /// slots hold immediates at this (natively-executed) return site.
    pub(crate) fn do_return_masked(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        raw: Value,
        release_mask: u64,
    ) -> Option<Value> {
        retain(raw); // keep alive across this frame's teardown
        let finished = frames.pop().unwrap();
        if release_mask == u64::MAX {
            let n = self.module.protos[finished.proto as usize].num_registers as usize;
            for i in 0..n {
                release(regs[finished.base + i]);
            }
        } else {
            let mut m = release_mask;
            while m != 0 {
                let i = m.trailing_zeros() as usize;
                m &= m - 1;
                release(regs[finished.base + i]);
            }
        }
        for u in &finished.upvalues {
            release(*u);
        }
        regs.truncate(finished.base);
        // An operator-dispatch frame may post-process its result (`!=` negates `eq`'s bool; `< <= > >=`
        // map `compare`'s `Ordering`). When the transform replaces a heap value (an `Ordering`) with a
        // fresh `bool`, release the original's keep-alive reference so it is not leaked.
        let (v, replaced) = finished.ret_transform.apply(raw);
        if replaced {
            release(raw);
        }
        match frames.last() {
            Some(caller) => {
                // Transfer the retained reference into the caller's destination.
                let idx = caller.base + finished.ret_dst as usize;
                let old = regs[idx];
                regs[idx] = v;
                release(old);
                None
            }
            None => Some(v),
        }
    }

    /// Dispatch a first-class prelude builtin called indirectly. Reuses `call_builtin` (so the
    /// arity/error text matches the direct `CallBuiltin` path exactly), except `len` on a user
    /// object, which re-enters that object's `Length` (`len`) method — mirroring the `CallBuiltin`
    /// object case. Arguments are borrowed; the result is freshly owned.
    fn call_native_fn(
        &mut self,
        func: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if func == Builtin::Len && args.len() == 1 && args[0].is_object() {
            let recv = args[0];
            if let Some(proto) = self.method_proto(&recv.shape().unwrap().name, "len") {
                let chunk = &self.module.protos[proto as usize];
                if chunk.num_params != 1 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "this method takes {} argument(s) but 0 were supplied",
                            chunk.num_params - 1
                        ),
                    ));
                }
                let (mut frames, mut regs) = self.pooled_run_stacks(chunk.num_registers as usize);
                retain(recv);
                regs[0] = recv;
                frames.push(Frame {
                    proto,
                    base: 0,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                    upvalues: Vec::new(),
                });
                return self.run(frames, regs);
            }
        }
        self.call_builtin(func, args, span)
    }

    /// Dispatch a prelude collection builtin. Arguments are borrowed (their registers retain
    /// ownership); the returned value is freshly owned.
    pub(crate) fn call_builtin(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match builtin {
            Builtin::Len => {
                self.check_arity(builtin, args, 1, span)?;
                let v = args[0];
                match v
                    .list_len()
                    .or_else(|| v.set_len())
                    .or_else(|| v.map_len())
                    .or_else(|| v.as_string().map(|s| s.chars().count()))
                {
                    Some(n) => Ok(Value::int(n as i64)),
                    None => Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`len` expects a list, map, or string, found {}",
                            v.type_name()
                        ),
                    )),
                }
            }
            Builtin::Map => {
                self.check_arity(builtin, args, 2, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`map` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // A `map(...)` whose result element type is packed (the checker marked this call span)
                // builds a flat result directly (P-PACK 2.6 category B): each mapped element is packed
                // into the buffer and its boxed object freed, so the result keeps the dense layout (and
                // downstream `[i].field` fusion) instead of materializing N boxed objects. A packed
                // input is read one element at a time (`packed_get`), so only one input element is live
                // at once too.
                if let Some(schema) = self.map_packed.get(&span).cloned() {
                    let input = args[0];
                    let n = input.list_len().expect("list");
                    let packed_input = input.is_packed_list();
                    let func = args[1];
                    let flat = Value::packed_list(schema, Vec::new()); // owned, refcount 1
                    let mut boxed: Option<Vec<Value>> = None;
                    for i in 0..n {
                        let element = if packed_input {
                            input.packed_get(i)
                        } else {
                            let e = input.list_get(i).expect("in bounds");
                            retain(e);
                            e
                        };
                        let out = match self.call_value(func, vec![element], span) {
                            Ok(v) => v,
                            Err(abort) => {
                                flat.release();
                                if let Some(b) = boxed {
                                    for v in b {
                                        release(v);
                                    }
                                }
                                return Err(abort);
                            }
                        };
                        if let Some(b) = &mut boxed {
                            b.push(out); // already in boxed mode
                        } else if flat.packed_push(out) {
                            release(out); // primitives copied into the buffer
                        } else {
                            // The mapped element did not pack (unreachable for a checker-marked site):
                            // demote the accumulated flat elements to a boxed vec, then continue boxed.
                            let count = flat.list_len().expect("packed");
                            let mut b = Vec::with_capacity(n);
                            for j in 0..count {
                                b.push(flat.packed_get(j));
                            }
                            flat.release();
                            b.push(out); // owned (not copied) — transferred into the vec
                            boxed = Some(b);
                        }
                    }
                    return Ok(match boxed {
                        Some(b) => Value::list(b),
                        None => flat,
                    });
                }
                // Demote a packed list to a temporary boxed one (P-PACK 2.4); its elements are
                // borrowed for the per-element calls and the temporary is released afterward.
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let func = args[1];
                let mut result = Vec::with_capacity(items.len());
                let mut failed = None;
                for element in items {
                    retain(element); // transferred into the call
                    match self.call_value(func, vec![element], span) {
                        Ok(v) => result.push(v),
                        Err(abort) => {
                            failed = Some(abort);
                            break;
                        }
                    }
                }
                list.release();
                if let Some(abort) = failed {
                    for r in &result {
                        release(*r);
                    }
                    return Err(abort);
                }
                Ok(Value::list(result))
            }
            Builtin::Filter => {
                self.check_arity(builtin, args, 2, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`filter` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // A packed list stays *flat* (P-PACK 2.6): test each element (materialized only for the
                // predicate call, then consumed by it), record the indices that pass, and rebuild a new
                // packed buffer from those word-blocks — never demoting the whole list to boxed.
                if args[0].is_packed_list() {
                    let list = args[0];
                    let func = args[1];
                    let n = list.list_len().expect("packed list");
                    let mut kept: Vec<usize> = Vec::new();
                    for i in 0..n {
                        let element = list.packed_get(i); // owned (rc 1), consumed by the call
                        let verdict = self.call_value(func, vec![element], span)?;
                        match verdict.as_bool() {
                            Some(true) => kept.push(i),
                            Some(false) => {}
                            None => {
                                let type_name = verdict.type_name();
                                release(verdict);
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    span,
                                    format!(
                                        "`filter` predicate must return a bool, found {type_name}"
                                    ),
                                ));
                            }
                        }
                        release(verdict); // the bool verdict (an immediate) is no longer needed
                    }
                    return Ok(list.packed_select(&kept));
                }
                // Demote a packed list (P-PACK 2.4); elements are borrowed from the temporary, which
                // is released after the loop (a kept element is retained into the result first).
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let func = args[1];
                let mut result = Vec::new();
                let mut failed = None;
                for element in items {
                    retain(element); // transferred into the call
                    let verdict = match self.call_value(func, vec![element], span) {
                        Ok(v) => v,
                        Err(abort) => {
                            failed = Some(abort);
                            break;
                        }
                    };
                    match verdict.as_bool() {
                        Some(true) => {
                            retain(element); // the result list now owns it too
                            result.push(element);
                        }
                        Some(false) => {}
                        None => {
                            let type_name = verdict.type_name();
                            release(verdict);
                            failed = Some(self.error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!("`filter` predicate must return a bool, found {type_name}"),
                            ));
                            break;
                        }
                    }
                    release(verdict); // the bool verdict (an immediate) is no longer needed
                }
                list.release();
                if let Some(abort) = failed {
                    for r in &result {
                        release(*r);
                    }
                    return Err(abort);
                }
                Ok(Value::list(result))
            }
            Builtin::Sum => {
                self.check_arity(builtin, args, 1, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // Demote a packed list (P-PACK 2.4) to a temporary boxed one, sum its (numeric)
                // elements, then release the temporary. (A `List<packed struct>` would not type-check
                // for `sum`, but the materialize keeps the path uniform.)
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                let mut bad: Option<&'static str> = None;
                for element in &items {
                    // Floats take the float path; every other numeric is an int (matching the
                    // M0 tree-walker, which distinguishes `3` from `3.0`).
                    if let Some(f) = element.as_float() {
                        any_float = true;
                        float_total += f;
                    } else if let Some(i) = element.as_int() {
                        int_total = int_total.wrapping_add(i);
                    } else {
                        bad = Some(element.type_name());
                        break;
                    }
                }
                list.release();
                if let Some(type_name) = bad {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects numeric elements, found {type_name}"),
                    ));
                }
                Ok(if any_float {
                    Value::float(float_total + int_total as f64)
                } else {
                    Value::int(int_total)
                })
            }
            // `assert(cond)` / `assert(cond, msg)` — mirrors the tree-walker (`Builtin::Assert`): a
            // false condition aborts with the same `Panic` diagnostic `panic` raises, a true one
            // yields unit. The condition must be `bool`; a non-bool is a `TypeMismatch`. Messages use
            // `display()` (as `Op::Panic` does), so the failure text is byte-identical across the
            // differential.
            Builtin::Assert => {
                if args.len() != 1 && args.len() != 2 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`assert` expects 1 or 2 arguments, found {}", args.len()),
                    ));
                }
                let Some(cond) = args[0].as_bool() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`assert` expects a bool, found {}", args[0].display()),
                    ));
                };
                if cond {
                    Ok(Value::unit())
                } else {
                    let message = match args.get(1) {
                        Some(msg) => format!("assertion failed: {}", msg.display()),
                        None => "assertion failed".to_string(),
                    };
                    Err(self.error(DiagnosticCode::Panic, span, message))
                }
            } // (The whole `Builtin` orchestration family — `task` at higher-order-abi H0/H2,
              // `http.serve` at H3, `signal`/`computed`/`effect` at H5 — migrated to the
              // registry's `NativeCtx` dispatch: `noeta-stdlib/src/{task,serve,reactive}.rs`,
              // reached via `call_ctx_function`/`call_ctx_type_method`. Only the language-level
              // collection builtins and `assert` remain here.)
        }
    }

    fn check_arity(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` takes {expected} argument(s) but {} were supplied",
                    builtin.name(),
                    args.len()
                ),
            ))
        }
    }
}
