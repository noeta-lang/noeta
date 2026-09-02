//! The tier-0 **dispatch loop**: [`Vm::run`] (the frame-stack driver) and
//! [`Vm::dispatch`] — deliberately ONE function containing the whole op match
//! (splitting it was assessed and declined for jump-table codegen and
//! cohesion;) — plus its register
//! helpers (`set_reg`, `reserve_window`, [`ArgBuf`]). Moved verbatim from the
//! crate root purely to shrink `lib.rs` — no behavior change.

use crate::*;

/// What an `Op::Invoke` resolved its runtime name to. The two arms are executed by *different*
/// mechanisms — a prototype is pushed onto the current frame stack, a free function re-enters
/// through the first-class-callee path — so keeping them apart here means the resolution block
/// stays a pure decision and the execution block cannot accidentally run one as the other.
enum InvokeTarget {
    /// A method or associated-function prototype. `is_assoc` decides whether register 0 receives
    /// the receiver or stays unit; either way the callee reserves that register.
    Proto { proto: u32, is_assoc: bool },
    /// A top-level function value read from its global slot (the two-argument form). Reserves no
    /// `self` register, and may carry upvalues, so it does not fit the prototype path.
    Free(Value),
    /// A method a `@derive`d built-in trait makes callable (`noeta_ast::derive::
    /// DERIVED_BUILTIN_METHODS`): `to_string`/`eq`/`compare` on a type that derives the trait but
    /// writes no body for it. There is no prototype to push — the routine runs synchronously — so
    /// the reflective door reaches the same member set a direct `x.to_string()` does.
    Derived(&'static noeta_ast::derive::DerivedBuiltinMethod),
}

/// How a [`Vm::cold_op`] arm left the dispatch loop — the three exits the arms already had,
/// named so an outlined arm can express them across a function boundary.
enum ColdStep {
    /// Keep interpreting this frame at the given `pc` (an arm that ran to completion; almost
    /// always `pc + 1`, and an arm that jumps sets its own target).
    Next(usize),
    /// Control transferred to a different frame — re-derive the register window (`continue
    /// 'reload`).
    Reload,
    /// The bottom frame produced a value; `dispatch_inner` returns it.
    Done(Value),
}

impl<'m> Vm<'m> {
    /// Run a frame stack until its bottom frame returns (`Return`) or the program/function
    /// halts (an implicit unit return). Returns the produced value, which the caller owns.
    /// On abort, every register still owned by a frame left on the stack is released here.
    pub(crate) fn run(
        &mut self,
        mut frames: Vec<Frame>,
        mut regs: Vec<Value>,
    ) -> Result<Value, Abort> {
        self.run_depth += 1;
        // Give the register stack generous headroom up front: a native direct call only
        // fires when the callee window fits without reallocating (so the caller's register pointer
        // stays valid), so a pre-reserved buffer keeps common recursion on the fast path. A deeper
        // stack simply reallocates once and the direct-call check re-passes at the new capacity, so
        // this only affects performance, never correctness — and is a no-op without the `jit` feature.
        // Outermost run only (audit-1 finding 5): a re-entrant entry — a closure applied per mapped
        // element — must not pay a 64 KB reserve per call; its pooled stack grows once and stays.
        #[cfg(feature = "jit")]
        if self.run_depth == 1 {
            regs.reserve(8192usize.saturating_sub(regs.len()));
        }
        let result = self.dispatch(&mut frames, &mut regs);
        self.run_depth -= 1;
        if result.is_err() {
            // Capture this stack segment for the abort traceback, innermost frame first, before the
            // teardown below reclaims anything. Costs nothing until an abort actually happens.
            //
            // Locations: a **caller** frame's saved `pc` is its resume point (a call saves `pc + 1`),
            // so `pc - 1` is the call op and the line table resolves it to the call site. The
            // **innermost** frame's saved `pc` is stale (it is only synced at calls), so its location
            // comes from the abort's just-recorded diagnostic — but only for the *first* captured
            // segment: when the abort climbs out of a re-entrant run (a closure called from inside a
            // builtin), the outer segment's top frame has no known abort site, and a stale line would
            // mislead; it gets `None` (name only).
            let first_segment = self.out.abort_trace.is_empty();
            for (fi, frame) in frames.iter().enumerate().rev() {
                let chunk = &self.module.protos[frame.proto as usize];
                let innermost = fi + 1 == frames.len();
                let span = if innermost {
                    first_segment
                        .then(|| self.out.diagnostics.last().map(|d| d.span))
                        .flatten()
                } else {
                    chunk.line_span(frame.pc.saturating_sub(1))
                };
                self.out.abort_trace.push(TraceFrame {
                    name: chunk.name.clone(),
                    span,
                });
            }
            // A panic unwinds the live frames. Before reclaiming their memory, fire
            // the `destruct` of every live destructor-bearing frame local — innermost frame first,
            // reverse-construction within each (the `frame_locals` list reversed) — so an aborting
            // program destroys its abandoned values deterministically (spec §6). This matches the
            // tree-walker, which fires each aborted scope's `drain_reverse` as the abort climbs the
            // call stack. Each fired register is cleared to `unit`, so the plain release below (which
            // also reclaims temporaries, never destructor-fired in either backend) never double-frees.
            for fi in (0..frames.len()).rev() {
                let f_base = frames[fi].base;
                let proto = frames[fi].proto as usize;
                let count = self.module.protos[proto].frame_locals.len();
                for idx in (0..count).rev() {
                    let reg = self.module.protos[proto].frame_locals[idx] as usize;
                    let v = std::mem::replace(&mut regs[f_base + reg], Value::unit());
                    self.release_value(v);
                }
            }
            // Release each live frame's register window from the shared stack (P-VMT-FRAME). A frame
            // owns `regs[base .. base + num_registers]`; the windows partition the stack, so this
            // releases every register exactly once.
            for frame in &frames {
                let n = self.module.protos[frame.proto as usize].num_registers as usize;
                for i in 0..n {
                    release(regs[frame.base + i]);
                }
                for u in &frame.upvalues {
                    release(*u);
                }
            }
        }
        // Return the (now fully released) stacks to the re-entrant pool (finding 5) so the next
        // re-entry pops them instead of allocating. On the `Ok` path both are already empty (the
        // bottom frame's return truncated to base 0); on abort the teardown above released every
        // value, so clearing loses nothing. Capacity caps keep a one-off deep run (or the
        // JIT-reserved outermost stack) from pinning memory, exactly like `ctx_table_pool`.
        if self.reentry_pool.len() < 8 && regs.capacity() <= 16384 && frames.capacity() <= 1024 {
            frames.clear();
            regs.clear();
            self.reentry_pool.push((frames, regs));
        }
        result
    }

    /// The dispatch loop's entry: stage the per-run inline-cache tables (from the pool — audit-1
    /// finding 5) around [`Vm::dispatch_inner`]. Returns `Ok(value)` once the bottom frame returns
    /// (the stack is then empty), or `Err(Abort)` with the stack left intact for [`Vm::run`] to
    /// release.
    fn dispatch(&mut self, frames: &mut Vec<Frame>, regs: &mut Vec<Value>) -> Result<Value, Abort> {
        // Per-run inline caches, one slot per cacheable call site (`LoadField`/`CallMethod`),
        // indexed by the op's `cache` field. Each entry memoizes the last receiver shape and the
        // resolved field-slot / method prototype; a hit is a pointer compare against the cached
        // shape, skipping the field-name scan / `(type, method)` hashmap lookup. A local (not a
        // `self` field) so it neither borrows `self` in the loop nor leaks across runs; holding the
        // `&'static Shape` keeps the cached shape alive, so the pointer key can never alias a freed
        // shape. Pooled (finding 5): a re-entrant entry pops the spare pair instead of allocating
        // two vectors sized to the whole module's cache-slot count; the entries are cleared on
        // exit, so a run always starts cold.
        let (mut caches, mut extern_caches) = self.cache_pool.pop().unwrap_or_default();
        caches.resize(self.module.cache_slots as usize, None);
        // Extern-method route cache: per `CallMethod` site, the resolved routing for an
        // extern receiver, keyed by the extern type's name pointer (a registry `&'static str`, a
        // stable identity). A hit is one heap probe + one pointer compare — no registry scans on
        // the `signal.get()`/`.set()` hot paths.
        extern_caches.resize(self.module.cache_slots as usize, None);
        let result = self.dispatch_inner(frames, regs, &mut caches, &mut extern_caches);
        // Cleared before pooling — a resolution must never carry across runs (a hot-swap or
        // fragment install between entries can rebind a method).
        caches.clear();
        extern_caches.clear();
        if self.cache_pool.len() < 8 {
            self.cache_pool.push((caches, extern_caches));
        }
        result
    }

    /// The dispatch loop proper — deliberately ONE function containing the whole op match (see
    /// the module header). `caches`/`extern_caches` are this entry's inline-cache tables, staged
    /// by [`Vm::dispatch`].
    fn dispatch_inner(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        caches: &mut Vec<MethodCacheEntry>,
        extern_caches: &mut [ExternCacheEntry],
    ) -> Result<Value, Abort> {
        // S3 dispatch window (P-VMT-DISP). The interpreter is two nested loops. The OUTER `'reload`
        // loop re-derives the active frame's register window — its base, prototype (`chunk`), and
        // starting `pc` — and is re-entered ONLY when control transfers to a *different* frame: a
        // call pushes one, a return / short-circuiting `?` pops one, each ending its arm with
        // `continue 'reload`. Within a frame the INNER loop runs straight-line: an op advances the
        // local `pc` and loops; a jump assigns it; neither re-indexes `frames` nor re-bounds-checks
        // the prototype table, which is what pinned the empty-loop floor at ~80 ns/iter before this
        // slice. `fbase`/`chunk` are immutable for the frame's lifetime — the only way to get a new
        // window is a new outer iteration, so a transfer *cannot* silently forget to reload. The
        // current frame's window is `regs[fbase .. fbase + chunk.num_registers]` (P-VMT-FRAME) and
        // every operand access below is `regs[fbase + i]` (`fbase`, not `base`, to avoid colliding
        // with ops that carry their own `base` field). `chunk` borrows `*module` (an `&'m Module`
        // copied out of `self`), so it is independent of the `&mut self` the arms use.
        'reload: loop {
            // Safepoint-GC poll at the frame transfer (memory-management 6.x): one predicted
            // thread-local-bool read when idle. A frame transfer is a safe point by construction —
            // every active register window holds exactly its owned references (the same invariant
            // the abort teardown releases by), so the full root set is enumerable.
            if noeta_value::safepoint_gc_pending() {
                self.maybe_safepoint_gc(frames, regs);
            }
            // **Cancellation poll at the frame transfer** (isolate-cancel), paired with the
            // back-edge poll in `osr_backedge!` below: together they mean every path a worker
            // isolate can take through bytecode — a call, a return, a loop — reaches a
            // cancellation check, so a compute-bound isolate is genuinely cancellable rather than
            // merely reported as such. A frame transfer is already a safe point by construction
            // (see the GC poll above); unwinding from here releases every live register exactly as
            // any other abort does. Costs one null test on a cached field outside a worker, where
            // `cancel_flag` is `None`.
            if self.cancel_requested() {
                return Err(self.observe_cancel());
            }
            // Re-read the module each frame transfer, NOT once per dispatch: a debug-console
            // fragment install ([`Vm::install_fragment`]) swaps
            // `self.module` to an extended snapshot mid-run, and the next frame must resolve
            // against the newest module — an escaped fragment closure's proto index only exists
            // there. Every snapshot is a stable-prefix superset, so a frame that started under an
            // older module re-derives byte-identical code here. One field load per call/return
            // (A/B-benched: noise); the copied-out `&'m Module` keeps `chunk` independent of the
            // `&mut self` the arms use, exactly as before.
            let module = self.module;
            // Fragment code can carry inline-cache slots past the base module's count — grow on
            // demand (never shrinks; a fresh slot starts cold). A no-op compare on non-debug runs.
            if caches.len() < module.cache_slots as usize {
                caches.resize(module.cache_slots as usize, None);
            }
            // Hover purity chokepoint: a hover fragment runs as a single wrapper frame; every
            // way of running user code — a call, an object's `Index` impl, a user ordering method —
            // pushes a second frame, which re-enters `'reload` here. Refuse it instead of running.
            // `pure_eval` is false on every non-hover run (one predicted branch per frame transfer).
            if self.pure_eval && frames.len() > 1 {
                let span = module.protos[frames[frames.len() - 1].proto as usize]
                    .line_span(0)
                    .unwrap_or_else(|| Span::empty_at(0));
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "hover stays read-only — evaluating this expression would run code \
                     (use a watch or the debug console)"
                        .to_string(),
                ));
            }
            let top = frames.len() - 1;
            let fbase = frames[top].base;
            let proto = frames[top].proto as usize;
            let chunk = &module.protos[proto];
            let mut pc = frames[top].pc;
            // Tier-0/tier-1 dispatch. Only at a fresh frame entry (`pc == 0`): a return-pop
            // re-enters `'reload` with the caller's saved `pc > 0`, and an in-frame jump never leaves
            // the inner loop, so `pc == 0` is exactly "this frame is starting". A compiled prototype
            // may run the whole frame in native code; a prototype with no compiled entry bails, so
            // control falls straight through to the interpreter below (byte-identical).
            // Fire at every frame `'reload`, not only fresh entries: after a native `Call`'s callee
            // returns, the interpreter re-enters the caller at its resume pc and native execution
            // picks up there (resume-native). `entry_pc = pc` is 0 for a fresh frame or the saved
            // resume pc otherwise; the compiled code jumps to that block (or bails if it has no entry
            // for it).
            #[cfg(feature = "jit-rt")]
            if self.native_dispatch_armed() {
                match self.jit_enter(proto, frames, regs, fbase, pc) {
                    // Not compiled → interpret as usual.
                    None => {}
                    // Native code ran the frame to a bail point and left the register window in the
                    // state the interpreter expects at `resume`; continue interpreting there.
                    Some(JitOutcome::Bail(resume)) => {
                        // Bail histogram (`--jit-stats`): count the site. Off (`None`) on every
                        // ordinary run — one predicted branch per bail event.
                        if let Some(counts) = self.tier1.jit_bail_counts.as_mut() {
                            *counts.entry((proto as u32, resume as u32)).or_insert(0) += 1;
                        }
                        pc = resume;
                    }
                    // A native `Call` pushed the callee frame — run it.
                    Some(JitOutcome::Called) => continue 'reload,
                    // A native `Return` transferred to the caller and popped this frame — re-derive
                    // the caller and continue.
                    Some(JitOutcome::Returned) => continue 'reload,
                    // The bottom frame returned natively — yield its value.
                    Some(JitOutcome::Halted) => {
                        return Ok(std::mem::replace(&mut self.tier1.jit_ret, Value::unit()));
                    }
                    // The frame aborted inside native code (a diagnostic is recorded).
                    Some(JitOutcome::Abort) => return Err(Abort),
                }
            }
            // OSR back-edge trigger: a taken backward branch to `target` is a loop
            // back-edge. When the JIT is armed (real-host path only — `self.jit` is `None` on the
            // sandbox/differential path, so this is a single predicted branch there) and the branch
            // goes backward, count it; once the prototype is hot, compile it and re-enter native at the
            // loop header by saving `pc` and reloading. `$target` is evaluated against the current `pc`
            // (the branch's own location) *before* `pc` is reassigned to it.
            macro_rules! osr_backedge {
                ($target:expr) => {
                    // Safepoint-GC poll at the taken loop back-edge (memory-management 6.x): the
                    // other half of the poll placement (see the `'reload` poll above), so a loop
                    // that never calls still reaches a safepoint each iteration. One predicted
                    // thread-local-bool read when idle.
                    if ($target as usize) <= pc && noeta_value::safepoint_gc_pending() {
                        self.maybe_safepoint_gc(&*frames, &*regs);
                    }
                    // Cancellation poll at the taken loop back-edge (isolate-cancel): the other
                    // half of the placement (see the `'reload` poll above), so a worker isolate
                    // spinning in a loop that never calls still reaches a cancellation check every
                    // iteration. Outside a worker `cancel_flag` is `None` and this is one predicted
                    // null test.
                    if ($target as usize) <= pc && self.cancel_requested() {
                        frames[top].pc = pc;
                        return Err(self.observe_cancel());
                    }
                    #[cfg(feature = "jit")]
                    {
                        let _osr_t = $target as usize;
                        if _osr_t <= pc
                            && (self.tier1.jit.is_some() || self.tier1.jit_service.is_some())
                            && self.jit_osr_backedge(proto, _osr_t)
                        {
                            frames[top].pc = _osr_t;
                            continue 'reload;
                        }
                    }
                };
            }
            loop {
                // Tooling seams (`noeta profile` / `noeta dap`), OUTLINED (perf/outline-cold).
                //
                // Both consults happen before every instruction and both are `None` on every
                // ordinary run, so what is left in the hot loop is exactly the two predicted
                // branches they always were. Their *bodies* — a `DebugView` over the whole live
                // stack, the pause state machine, watch/console evaluation, and the strand read
                // they both wanted — used to sit inline and hold live state across the op match,
                // which every arm then paid for in its register allocation. They are now `#[cold]`
                // `#[inline(never)]` calls; the semantics, the ordering (profiler first, then
                // debugger), and the pc sync inside each are unchanged.
                if self.profiler.is_some() {
                    self.profile_before_op(module, frames, regs, top, pc);
                }
                if self.debugger.is_some() {
                    self.debug_before_op(module, frames, regs, top, proto, pc)?;
                }
                // Every prototype ends with `Halt`, so `pc` never runs off the end — index directly
                // instead of the `.get()` guard the pre-S3 loop used. A call keeps `fbase` on the
                // *caller* until `continue 'reload`, so a call op reads its arguments first.
                let op = &chunk.code[pc];
                match op {
                    Op::LoadConst { dst, k } => {
                        let v = materialize(&chunk.consts[*k as usize]);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::Move { dst, src } => {
                        let v = regs[fbase + *src as usize];
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::LoadGlobal { dst, global, span } => {
                        // Direct slot index — no name hashing (P-VMT-GSLOT). An unbound slot holds the
                        // `Value::unbound` sentinel (P-JIT globals); every other value is a real binding.
                        let v = self.persist.globals[global.0 as usize];
                        if v.is_unbound() {
                            // In a worker isolate, an unbound slot may be a global the parent could
                            // not ship (isolates I.4b) — a `class` (reference identity), a captured
                            // closure, a `Local` channel. Name it + its type + the fix, instead of
                            // the misleading "cannot find `x`" (the global clearly exists in source).
                            if let Some(ty) = self.isolates.unshippable_globals.get(&global.0) {
                                return Err(self.error(
                                    DiagnosticCode::NotSend,
                                    *span,
                                    format!(
                                        "the global `{}` of type `{ty}` cannot be shared with an \
                                         isolate — only value types cross an isolate boundary (a \
                                         reference `class` has identity, a captured closure and a \
                                         cooperative channel hold heap state). Make it a value \
                                         type, or pass the value-type data it holds to the isolate \
                                         as arguments.",
                                        module.global_name(*global)
                                    ),
                                ));
                            }
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::StoreGlobal { global, src } => {
                        // Transfer ownership from the (dead) source temporary into the global,
                        // rather than retaining a duplicate. This keeps the reference count equal
                        // to the tree-walker's direct-binding model — a lingering temporary would
                        // otherwise inflate the count and hide a reassigned value's last reference,
                        // suppressing its destructor.
                        let v = std::mem::replace(&mut regs[fbase + *src as usize], Value::unit());
                        let old =
                            std::mem::replace(&mut self.persist.globals[global.0 as usize], v);
                        if old.is_unbound() {
                            // First binding of this slot: record it for reverse-order destruction.
                            self.persist.global_order.push(global.0);
                        } else {
                            // Reassigning: the previous value is dropped here, running its destructor
                            // if this was its last reference.
                            self.release_value(old);
                        }
                        pc += 1;
                    }
                    Op::TakeGlobal { dst, global, span } => {
                        // Move the global's value into `dst`, leaving `unit` — no retain, so the single
                        // owning reference transfers and a following `ConcatInPlace` can see uniqueness.
                        // An unbound slot raises E0005 (and is left unbound); a bound slot stays bound
                        // (to `unit`), matching the pre-refactor `Option` semantics.
                        if self.persist.globals[global.0 as usize].is_unbound() {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        let v = std::mem::replace(
                            &mut self.persist.globals[global.0 as usize],
                            Value::unit(),
                        );
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::Drop { reg, relevant } => {
                        // Release a dead binding/temporary at its last use and clear it to `unit` (so
                        // `set_reg`/teardown later release `unit`, never double-freeing). This frees the
                        // value promptly, restoring an accumulator's unique ownership. When the IR marked
                        // the drop destructor-relevant, route it through `release_value` so a
                        // `destruct` block fires here if this is the final owning reference; otherwise the
                        // value provably reaches no destructor and the plain `release` is used.
                        let v = std::mem::replace(&mut regs[fbase + *reg as usize], Value::unit());
                        if *relevant {
                            self.release_value(v);
                        } else {
                            release(v);
                        }
                        pc += 1;
                    }
                    Op::ConcatInPlace { dst, lhs, rhs, .. } => {
                        let l = regs[fbase + *lhs as usize];
                        let r = regs[fbase + *rhs as usize];
                        // `lhs` is consumed: clear its register *without* releasing (a direct overwrite,
                        // not `set_reg`), so the refcount below still counts the accumulator's reference
                        // and the single owner is transferred into the result. This also makes a
                        // `dst == lhs` store safe (the old occupant is now `unit`, not the live list).
                        regs[fbase + *lhs as usize] = Value::unit();
                        let result = concat_in_place(l, r);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::MakeCell { dst, src } => {
                        // Box the value into a fresh cell, which owns one reference to it.
                        let v = regs[fbase + *src as usize];
                        retain(v);
                        set_reg(regs, fbase, *dst, Value::cell(v));
                        pc += 1;
                    }
                    Op::CellGet { dst, cell } => {
                        let v = regs[fbase + *cell as usize].cell_get();
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::CellSet { cell, src } => {
                        // `cell_set` retains the new occupant and releases the old internally.
                        let v = regs[fbase + *src as usize];
                        regs[fbase + *cell as usize].cell_set(v);
                        pc += 1;
                    }
                    Op::UpvalueGet { dst, index } => {
                        let v = frames[top].upvalues[*index as usize].cell_get();
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::UpvalueSet { index, src } => {
                        let v = regs[fbase + *src as usize];
                        frames[top].upvalues[*index as usize].cell_set(v);
                        pc += 1;
                    }
                    // A `List<packed>` literal: pack each element into a flat raw-primitive
                    // buffer (no boxed objects, no retains — the words are copied), then the element
                    // temporaries are released by the following compiler-emitted drops, exactly as for
                    // `MakeList`'s consumed operands. If any element fails to pack (a shape the schema
                    // does not expect — not reachable for a well-typed marked site), fall back to a boxed
                    // list that retains each element, staying consistent with those drops.
                    // A tuple builds exactly like a list: retain each element into
                    // the aggregate, which owns one reference to each.
                    // Positional projection `receiver.N`: read the Nth element of the tuple, retaining it
                    // into `dst`. The index is in range by construction (the checker verified it).
                    Op::ListLen { dst, src, span } => {
                        // After `IterSnapshot`, `src` is a list for the list/map paths; the only way it
                        // is not is an `Iterable::iter` that returned a non-list, reported here (E0007),
                        // matching the tree-walker's `exec_for`.
                        let v = regs[fbase + *src as usize];
                        match v.list_len() {
                            Some(n) => {
                                set_reg(regs, fbase, *dst, Value::int(n as i64));
                                pc += 1;
                            }
                            // `iter()` returned a `next`-driven user iterator object (the
                            // Iterable → member-handle composition): drain it into the snapshot
                            // register, exactly as the tree-walker's `iter_elements` does, so the
                            // loop's `ListGet` reads the materialized elements.
                            None if self.has_user_next(v) => {
                                let list = self.drain_next_object(v, *span)?;
                                let n = list.list_len().expect("the drain returns a list");
                                set_reg(regs, fbase, *src, list);
                                set_reg(regs, fbase, *dst, Value::int(n as i64));
                                pc += 1;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("`iter` must return a list, found {}", v.type_name()),
                                ));
                            }
                        }
                    }
                    Op::ListGet { dst, list, index } => {
                        let element = list_get_retained(
                            regs[fbase + *list as usize],
                            regs[fbase + *index as usize],
                        )
                        .expect("the loop keeps the (int) index in bounds");
                        set_reg(regs, fbase, *dst, element);
                        pc += 1;
                    }
                    // Streaming `for` step (Track I.2): advance the iterator, binding the element + a bool
                    // continue flag. A `map`/`filter` closure runs here (via `iter_for_next`), so it can
                    // abort. `set_reg` releases the previous element / flag each iteration.
                    Op::IterForNext {
                        iter,
                        elem,
                        has,
                        span,
                    } => {
                        let it = regs[fbase + *iter as usize];
                        match self.iter_for_next(it, *span)? {
                            Some(element) => {
                                set_reg(regs, fbase, *elem, element);
                                set_reg(regs, fbase, *has, Value::bool(true));
                            }
                            None => {
                                set_reg(regs, fbase, *elem, Value::unit());
                                set_reg(regs, fbase, *has, Value::bool(false));
                            }
                        }
                        pc += 1;
                    }
                    Op::CallBuiltin {
                        dst,
                        builtin,
                        args,
                        span,
                    } => {
                        // A user object lights up the `Length` trait: `len(o)` dispatches to its `len`
                        // method, which runs bytecode, so it is pushed as a call frame rather than
                        // handled by the synchronous `call_builtin`. (Matches the tree-walker's
                        // `Builtin::Len` object case.)
                        if *builtin == Builtin::Len && args.len() == 1 {
                            let recv = regs[fbase + args[0] as usize];
                            if recv.is_object()
                                && let Some(proto) =
                                    self.method_proto(&recv.shape().unwrap().name, "len")
                            {
                                let callee_chunk = &module.protos[proto as usize];
                                if callee_chunk.num_params != 1 {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "this method takes {} argument(s) but 0 were supplied",
                                            callee_chunk.num_params - 1
                                        ),
                                    ));
                                }
                                self.push_callee_frame(
                                    frames,
                                    regs,
                                    top,
                                    proto,
                                    Some(recv),
                                    &[],
                                    &[],
                                    *dst,
                                    RetTransform::None,
                                    pc + 1,
                                    // A fixed-arity protocol call — no labels reach it.
                                    None,
                                    *span,
                                )?;
                                continue 'reload;
                            }
                        }
                        // Builtins borrow their arguments (the registers keep ownership); the
                        // result is a fresh owned value.
                        let arg_vals = ArgBuf::collect(args, regs, fbase);
                        let (dst, builtin, span) = (*dst, *builtin, *span);
                        let v = self.call_builtin(builtin, arg_vals.as_slice(), span)?;
                        set_reg(regs, fbase, dst, v);
                        pc += 1;
                    }
                    Op::CallMethod {
                        dst,
                        recv,
                        method,
                        args,
                        // A forwarding generic METHOD's type arguments (Axis A) — empty for every
                        // method call that forwards nothing, which is all but a handful. They are
                        // read only on the user-method arms below: no built-in, native or protocol
                        // receiver declares hidden slots.
                        type_args,
                        span,
                        cache,
                        reuse,
                        consume_key,
                        supplied,
                    } => {
                        // Resolve the interned method name once; every path below wants the `&str`.
                        let method = module.name(*method);
                        let v = regs[fbase + *recv as usize];
                        // Classify the receiver once (one heap dereference). Every rung below
                        // tests `hk` with an integer compare instead of re-probing the heap
                        // per candidate type — a deep rung (map/iter methods) used to pay a
                        // dereference for every rung above it.
                        let hk = v.heap_kind();
                        // In-place map self-update: a reuse-marked `m = m.set(k,v)` /
                        // `m = m.remove(k)` whose runtime receiver is actually a map consumes the receiver
                        // register and mutates the sole-owned backing buffer in place (an alias copies). A
                        // non-map receiver — a user method that happens to be named `set` — falls through to
                        // the ordinary dispatch below with the receiver intact.
                        if *reuse
                            && hk == Some(HeapKind::Map)
                            && let Some(map_method) = noeta_stdlib::MapMethod::from_name(method)
                            && matches!(
                                map_method,
                                noeta_stdlib::MapMethod::Set | noeta_stdlib::MapMethod::Remove
                            )
                        {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            // Consume the receiver: take its single reference out of the register without
                            // releasing (a direct overwrite, like `ConcatInPlace`), so the refcount below
                            // still counts the accumulator's reference and a `dst == recv` store is safe.
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.map_update_in_place(
                                v,
                                map_method,
                                method,
                                arg_values.as_slice(),
                                *consume_key,
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // In-place list self-update (`xs[i] = v` ⟶ `xs = xs.set(i, v)`): a uniquely-owned
                        // list overwrites slot `i` in place (O(1)) instead of copying the whole list.
                        if *reuse
                            && matches!(hk, Some(HeapKind::List | HeapKind::PackedList))
                            && method == "set"
                        {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.list_set_in_place(v, arg_values.as_slice(), *span)?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // In-place set self-update (`s = s.add(x)` / `s = s.remove(x)`): a uniquely-owned,
                        // canonically-ordered set binary-search-inserts/removes one element in its existing
                        // buffer instead of cloning + re-sorting the whole set.
                        if *reuse
                            && hk == Some(HeapKind::Set)
                            && let Some(set_method) = noeta_stdlib::SetMethod::from_name(method)
                            && matches!(
                                set_method,
                                noeta_stdlib::SetMethod::Add | noeta_stdlib::SetMethod::Remove
                            )
                        {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.set_update_in_place(
                                v,
                                set_method,
                                method,
                                arg_values.as_slice(),
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // Receiver-dispatching method routes, OUTLINED (perf/outline-cold).
                        //
                        // A native module, an extern value, an object or an enum resolves its
                        // method through a table and usually pushes a callee frame; those four
                        // routes were ~325 of this arm's ~430 lines, and they held the arm's whole
                        // operand set live across the dispatch loop. They now live in
                        // [`Vm::call_method_dispatch`] (`#[cold] #[inline(never)]`), moved
                        // verbatim, guarded by ONE receiver-kind test that replaces the four they
                        // each performed — so a built-in receiver (`xs.len()`, a string method, a
                        // map read) reaches the value-in/value-out chain below without entering
                        // them, exactly as it did before. `Ok(None)` means "not one of these four
                        // after all", the same fall-through the blocks always had.
                        if matches!(
                            hk,
                            Some(
                                HeapKind::NativeModule
                                    | HeapKind::Extern
                                    | HeapKind::Object
                                    | HeapKind::Enum
                            )
                        ) && let Some(step) = self.call_method_dispatch(
                            v,
                            hk,
                            method,
                            dst,
                            args,
                            type_args,
                            span,
                            cache,
                            supplied,
                            frames,
                            regs,
                            caches,
                            extern_caches,
                            module,
                            fbase,
                            top,
                            pc,
                        )? {
                            match step {
                                ColdStep::Next(next) => {
                                    pc = next;
                                    continue;
                                }
                                ColdStep::Reload => continue 'reload,
                                // These routes never end the program — they either write `dst` and
                                // advance, or transfer to a callee frame.
                                ColdStep::Done(_) => unreachable!(
                                    "a method-dispatch route cannot return the bottom frame"
                                ),
                            }
                        }
                        // Everything below the object/enum dispatch is a built-in method on a
                        // non-object receiver — value-in/value-out, factored into
                        // `call_builtin_method` (prelude-redesign MH.2) so an unbound method handle
                        // (`list.len` as a value) dispatches through the SAME branches by
                        // construction. Arguments are borrowed from the registers (which keep
                        // ownership; `ArgBuf` stages ≤8 inline — no method-call path allocates that
                        // did not before the extraction), and the receiver's one-shot `hk`
                        // classification is passed through so the helper's rungs keep main's
                        // integer-compare receiver tests (no re-deref per rung).
                        let arg_values = ArgBuf::collect(args, regs, fbase);
                        let (dst, span) = (*dst, *span);
                        let value =
                            self.call_builtin_method(v, hk, method, arg_values.as_slice(), span)?;
                        set_reg(regs, fbase, dst, value);
                        pc += 1;
                    }
                    Op::Index {
                        dst,
                        recv,
                        index,
                        span,
                    } => {
                        let v = regs[fbase + *recv as usize];
                        let idx = regs[fbase + *index as usize];
                        // `o[i]` on a user object lights up the `Index` trait: dispatch to `get`,
                        // pushing a call frame `[recv, index]` exactly like a method call. An object
                        // without an `Index` impl has no `get` method, so this reports the missing
                        // method — matching the tree-walker's `eval_index`.
                        if v.is_object() {
                            let type_name = &v.shape().unwrap().name;
                            let Some(proto) = self.method_proto(type_name, "get") else {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!("type `{type_name}` has no method `get`"),
                                ));
                            };
                            let callee_chunk = &module.protos[proto as usize];
                            if callee_chunk.num_params as usize != 2 {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this method takes {} argument(s) but 1 were supplied",
                                        callee_chunk.num_params - 1
                                    ),
                                ));
                            }
                            self.push_callee_frame(
                                frames,
                                regs,
                                top,
                                proto,
                                Some(v),
                                &[idx],
                                &[],
                                *dst,
                                RetTransform::None,
                                pc + 1,
                                // A fixed-arity protocol call — no labels reach it.
                                None,
                                *span,
                            )?;
                            continue 'reload;
                        }
                        // A built-in list addresses an element by integer position (bounds-checked).
                        if let Some(len) = v.list_len() {
                            let Some(i) = idx.as_int() else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("list index must be an int, found {}", idx.type_name()),
                                ));
                            };
                            if i < 0 || i as usize >= len {
                                return Err(self.error(
                                    DiagnosticCode::IndexOutOfBounds,
                                    *span,
                                    format!("index {i} out of bounds for list of length {len}"),
                                ));
                            }
                            set_reg(regs, fbase, *dst, list_element_retained(v, i as usize));
                            pc += 1;
                            continue;
                        }
                        // A map looks the value up by its string key; a missing key is `E0018`.
                        if v.is_map() {
                            // Borrow the key's `&str` for the lookup — no clone on the hot found path;
                            // the cold error paths clone only for their message.
                            match idx.with_str(|key| v.map_get(key)) {
                                Some(Some(element)) => {
                                    retain(element);
                                    set_reg(regs, fbase, *dst, element);
                                    pc += 1;
                                    continue;
                                }
                                Some(None) => {
                                    let key = idx.as_string().unwrap_or_default();
                                    return Err(self.error(
                                        DiagnosticCode::KeyNotFound,
                                        *span,
                                        format!("map has no key {key:?}"),
                                    ));
                                }
                                None => {
                                    // Not a string: an int keys directly, a
                                    // key-capable extern value probes through the contract,
                                    // a key-capable packed struct by its
                                    // content snapshot; anything else is the existing
                                    // type error.
                                    if let Some(i) = idx.as_int() {
                                        if let Some(element) =
                                            v.map_get_key(&noeta_stdlib::MapKey::Int(i))
                                        {
                                            retain(element);
                                            set_reg(regs, fbase, *dst, element);
                                            pc += 1;
                                            continue;
                                        }
                                        return Err(self.error(
                                            DiagnosticCode::KeyNotFound,
                                            *span,
                                            format!("map has no key {i}"),
                                        ));
                                    }
                                    if idx.is_extern()
                                        && idx
                                            .with_extern(noeta_stdlib::map_key::extern_key_capable)
                                    {
                                        if let Some(element) =
                                            idx.with_extern(|e| v.map_get_extern(e))
                                        {
                                            retain(element);
                                            set_reg(regs, fbase, *dst, element);
                                            pc += 1;
                                            continue;
                                        }
                                        return Err(self.error(
                                            DiagnosticCode::KeyNotFound,
                                            *span,
                                            format!("map has no key {}", idx.display()),
                                        ));
                                    }
                                    if let Some(k) = idx.packed_map_key() {
                                        if let Some(element) = v.map_get_key(&k) {
                                            retain(element);
                                            set_reg(regs, fbase, *dst, element);
                                            pc += 1;
                                            continue;
                                        }
                                        return Err(self.error(
                                            DiagnosticCode::KeyNotFound,
                                            *span,
                                            format!("map has no key {}", idx.display()),
                                        ));
                                    }
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "map index must be a string, found {}",
                                            idx.type_name()
                                        ),
                                    ));
                                }
                            }
                        }
                        // A string addresses a single character by position (bounds-checked),
                        // counting by Unicode scalar values to match `len`.
                        if let Some(s) = v.as_string() {
                            let Some(i) = idx.as_int() else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "string index must be an int, found {}",
                                        idx.type_name()
                                    ),
                                ));
                            };
                            let count = s.chars().count();
                            if i < 0 || i as usize >= count {
                                return Err(self.error(
                                    DiagnosticCode::IndexOutOfBounds,
                                    *span,
                                    format!("index {i} out of bounds for string of length {count}"),
                                ));
                            }
                            let ch = s.chars().nth(i as usize).unwrap().to_string();
                            set_reg(regs, fbase, *dst, Value::string(&ch));
                            pc += 1;
                            continue;
                        }
                        // A `bytes` buffer reads one byte as an `int` (0..=255). Borrowed in place
                        // through `with_bytes` — never `bytes_data`, which clones the whole buffer
                        // and would make a decode loop quadratic — and the read itself is the shared
                        // `noeta_stdlib::bytes_index`, so the bounds error matches the tree-walker.
                        if v.is_bytes() {
                            let Some(i) = idx.as_int() else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "bytes index must be an int, found {}",
                                        idx.type_name()
                                    ),
                                ));
                            };
                            let read = v
                                .with_bytes(|data| noeta_stdlib::bytes_index(data, i))
                                .expect("checked `is_bytes` above");
                            match read {
                                Ok(byte) => set_reg(regs, fbase, *dst, Value::int(byte)),
                                Err(error) => {
                                    return Err(self.std_dispatch_error(error, *span));
                                }
                            }
                            pc += 1;
                            continue;
                        }
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot index a value of type {}", v.type_name()),
                        ));
                    }
                    Op::IndexField {
                        dst,
                        recv,
                        index,
                        field,
                        span,
                    } => {
                        let field = module.name(*field);
                        let v = regs[fbase + *recv as usize];
                        let idx = regs[fbase + *index as usize];
                        // Fast path: a packed list decodes the one field's word(s) directly — no element
                        // materialization (the scalar-access win). Any miss (non-int index,
                        // out of range, or unknown field) falls through to the boxed index-then-load,
                        // which reproduces the exact diagnostics of the unfused `Index` + `LoadField`.
                        if v.is_packed_list()
                            && let Some(i) = idx.as_int()
                            && i >= 0
                            && let Some(value) = v.packed_field(i as usize, field)
                        {
                            set_reg(regs, fbase, *dst, value);
                            pc += 1;
                            continue;
                        }
                        // Fallback. The static type guarantees a `List`; bounds-check the index exactly as
                        // `Op::Index`'s list branch, then read the element's field exactly as
                        // `Op::LoadField`. A boxed element is borrowed (only its loaded field is retained
                        // into `dst`); a packed element reached here (unknown field — unreachable for a
                        // checker-fused site) is materialized owned and released after.
                        let Some(len) = v.list_len() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index a value of type {}", v.type_name()),
                            ));
                        };
                        let Some(i) = idx.as_int() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("list index must be an int, found {}", idx.type_name()),
                            ));
                        };
                        if i < 0 || i as usize >= len {
                            return Err(self.error(
                                DiagnosticCode::IndexOutOfBounds,
                                *span,
                                format!("index {i} out of bounds for list of length {len}"),
                            ));
                        }
                        let packed = v.is_packed_list();
                        let element = if packed {
                            v.packed_get(i as usize) // owned (rc 1)
                        } else {
                            v.list_get(i as usize).expect("bounds checked above") // borrowed
                        };
                        let slot = element.shape().and_then(|sh| sh.slot_of(field));
                        match slot.and_then(|s| element.slot_at(s)) {
                            Some(value) => {
                                retain(value);
                                if packed {
                                    release(element);
                                }
                                set_reg(regs, fbase, *dst, value);
                                pc += 1;
                            }
                            None => {
                                let err = if element.is_object() {
                                    self.error(
                                        DiagnosticCode::UnknownName,
                                        *span,
                                        format!(
                                            "type `{}` has no field `{field}`",
                                            element.shape().unwrap().name
                                        ),
                                    )
                                } else {
                                    self.error(
                                        DiagnosticCode::UnknownName,
                                        *span,
                                        format!("no field `{field}` on {}", element.type_name()),
                                    )
                                };
                                if packed {
                                    release(element);
                                }
                                return Err(err);
                            }
                        }
                    }
                    Op::MakeEnum {
                        dst,
                        shape,
                        args,
                        reflect,
                    } => {
                        let shape = self.persist.shapes[*shape as usize];
                        let mut data = Vec::with_capacity(args.len());
                        for &r in args.iter() {
                            let v = regs[fbase + r as usize];
                            retain(v);
                            data.push(v);
                        }
                        let value = Value::enum_value(shape, data);
                        // Stamp the reflected type onto a generic enum-variant construction so
                        // `type_of` recovers its type arguments after a `dyn` launder. Like an object's tag,
                        // an enum value's type is invariant, so it is never cleared.
                        if let Some(idx) = reflect {
                            value.set_reflect(Some(Rc::clone(
                                &self.persist.type_reprs[*idx as usize],
                            )));
                        }
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::LoadField {
                        dst,
                        obj,
                        field,
                        span,
                        cache,
                    } => {
                        let field = module.name(*field);
                        let v = regs[fbase + *obj as usize];
                        // Inline cache: a hit (the receiver's shape pointer matches the cached one) reads
                        // the memoized slot directly; a miss resolves `slot_of` and refreshes the cache.
                        // The hit check returns an owned slot so the `&caches[ci]` borrow ends before the
                        // miss path mutates the same entry.
                        let ci = *cache as usize;
                        let hit = match &caches[ci] {
                            Some((cs, slot))
                                if v.object_shape_ptr()
                                    == Some(std::ptr::from_ref::<Shape>(cs)) =>
                            {
                                Some(*slot as usize)
                            }
                            _ => None,
                        };
                        let cached_slot = match hit {
                            Some(slot) => Some(slot),
                            None => match v.shape() {
                                Some(sh) => sh.slot_of(field).inspect(|&s| {
                                    caches[ci] = Some((sh, s as u32));
                                }),
                                None => None,
                            },
                        };
                        match cached_slot.and_then(|s| v.slot_at(s)) {
                            Some(value) => {
                                retain(value);
                                set_reg(regs, fbase, *dst, value);
                                pc += 1;
                            }
                            None if v.is_object() => {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!(
                                        "type `{}` has no field `{field}`",
                                        v.shape().unwrap().name
                                    ),
                                ));
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!("no field `{field}` on {}", v.type_name()),
                                ));
                            }
                        }
                    }
                    Op::SetField {
                        dst,
                        obj,
                        field,
                        value,
                        reuse,
                        span,
                    } => {
                        let field = module.name(*field);
                        // The store (class in-place / struct COW / reuse) is shared with the tier-1 JIT
                        // leaf helper; a `false` return is the field-not-found error path.
                        if !self.set_field_fast(regs, fbase, *dst, *obj, field, *value, *reuse) {
                            let v = regs[fbase + *obj as usize];
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                if v.is_object() {
                                    format!(
                                        "type `{}` has no field `{field}`",
                                        v.shape().unwrap().name
                                    )
                                } else {
                                    format!("cannot assign field `{field}` on {}", v.type_name())
                                },
                            ));
                        }
                        pc += 1;
                    }
                    Op::Coalesce {
                        dst,
                        src,
                        fallback,
                        span,
                    } => {
                        let v = regs[fbase + *src as usize];
                        match try_classify(v) {
                            Some(TryOutcome::Success(inner)) => {
                                retain(inner);
                                set_reg(regs, fbase, *dst, inner);
                                pc += 1;
                            }
                            // Empty: jump to the fallback expression (which writes `dst`).
                            Some(TryOutcome::Empty) => pc = *fallback as usize,
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`??` expects a `Result` or `Option` on the left, found {}",
                                        v.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::Narrow {
                        dst,
                        src,
                        target,
                        dynamic,
                        some_shape,
                        none_shape,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let dyn_target = runtime_narrow_target(regs, fbase, *dynamic);
                        let target = dyn_target.as_ref().unwrap_or(target);
                        let result = if narrow_matches(v, target, &self.module.reflection) {
                            retain(v);
                            let shape = self.persist.shapes[*some_shape as usize];
                            Value::enum_value(shape, vec![v])
                        } else {
                            let shape = self.persist.shapes[*none_shape as usize];
                            Value::enum_value(shape, Vec::new())
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::IsType {
                        dst,
                        src,
                        target,
                        dynamic,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let dyn_target = runtime_narrow_target(regs, fbase, *dynamic);
                        let target = dyn_target.as_ref().unwrap_or(target);
                        let result =
                            Value::bool(narrow_matches(v, target, &self.module.reflection));
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    // `type_name::<T>()` over the enclosing generic type's parameter: read
                    // argument `index` off the receiver's reflected type tag.
                    // The fn-side twin: a FORWARDED `type_name::<T>()` reads the qualified name
                    // off the hidden slot's type-argument entry. Mirrors the tree-walker, and
                    // mirrors the dynamic `AttributesOf` arm — same slot, entry and field.
                    // Stamp a fresh constructor's instantiation onto its result (generic
                    // constructor reflection). The value was just returned by a call the checker
                    // proved builds it fresh, so writing the tag in place is unobservable to any
                    // other reference — there is none.
                    // The same stamp with the tag named at run time (generic-in-generic
                    // construction): the hidden type-argument slot in `slot` indexes the module's
                    // `type_args`, whose parallel `type_arg_reprs` entry is the interned tag. The
                    // reference backend resolves the identical index in the identical table, which is
                    // what makes the two agree; a corrupt slot or an entry with no reflection
                    // projection leaves the value untagged (the head-only fallback), never guesses.
                    Op::MatchInt { src, value, fail } => {
                        if regs[fbase + *src as usize].as_int() == Some(*value) {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchStr { src, value, fail } => {
                        if regs[fbase + *src as usize].as_string().as_deref()
                            == Some(module.name(*value))
                        {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchBool { src, value, fail } => {
                        if regs[fbase + *src as usize].as_bool() == Some(*value) {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchVariant {
                        src,
                        type_name,
                        variant,
                        arity,
                        fail,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let builtin_carrier =
                            v.shape().is_some_and(|s| s.name.as_str() == "Result");
                        let matches = v.is_enum()
                            && v.shape().is_some_and(|shape| {
                                shape.variant.as_deref() == Some(module.name(*variant))
                                    && type_name
                                        .is_none_or(|t| module.name(t) == shape.name.as_str())
                            })
                            && v.enum_data().is_some_and(|d| {
                                d.len() == *arity as usize
                                    || crate::lifecycle::unit_payload_match(
                                        builtin_carrier,
                                        d.len(),
                                        *arity as usize,
                                    )
                            });
                        if matches {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    // A tuple pattern test: `src` must be a tuple of exactly
                    // `arity` elements. The elements are then read with `TupleIndex` for sub-patterns.
                    Op::MatchTuple { src, arity, fail } => {
                        let v = regs[fbase + *src as usize];
                        let matches = v
                            .tuple_items()
                            .is_some_and(|items| items.len() == *arity as usize);
                        if matches {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::ExtractField { dst, src, index } => {
                        // Past the end only on the `unit_payload_match` path — a payload-less `Ok()`
                        // reached through an `Ok(v)` pattern — where the payload is `void` and `unit`
                        // is the whole of it.
                        let element = regs[fbase + *src as usize]
                            .enum_data()
                            .and_then(|d| d.into_iter().nth(*index as usize))
                            .unwrap_or_else(Value::unit);
                        retain(element);
                        set_reg(regs, fbase, *dst, element);
                        pc += 1;
                    }
                    Op::MatchFail { src, span } => {
                        let shown = regs[fbase + *src as usize].display();
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("no match arm matched the value {shown}"),
                        ));
                    }
                    Op::Unary { op, dst, src, span } => {
                        match apply_unary(*op, regs[fbase + *src as usize]) {
                            Ok(v) => {
                                // `...xs` (spread) returns the source value unchanged, so the result
                                // aliases a live heap reference — retain it before `set_reg` releases
                                // the old occupant of `dst` (which may be `src`); mirrors `Op::Move`.
                                // `Neg`/`Not` build a **fresh** value that already owns its reference
                                // — an `int` outside the 48-bit inline range heap-boxes, so retaining
                                // one strands it. They take `apply_unary`'s result straight into
                                // `dst`, exactly as the arithmetic path takes `apply_binary`'s.
                                if *op == UnaryOp::Spread {
                                    retain(v);
                                }
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
                        }
                    }
                    Op::Binary {
                        op,
                        dst,
                        a,
                        b,
                        span,
                    } => {
                        let left = regs[fbase + *a as usize];
                        let right = regs[fbase + *b as usize];
                        // Operator-trait dispatch on a user object or enum value (the unified body's
                        // in-body `impl` blocks are uniform across kinds): an
                        // arithmetic/concat operator routes to its trait method and uses the result
                        // directly; `==`/`!=` route to `Equatable::eq` (`!=` negating via the frame's
                        // return transform); `< <= > >=` route to `Comparable::compare`. The method table
                        // is keyed by the value's shape name, identical for objects and enums. Built-in
                        // semantics apply otherwise; the checker guarantees a dispatched method's arity.
                        let dispatch = if left.is_object() || left.is_enum() {
                            let type_name = &left.shape().unwrap().name;
                            if let Some(method_name) = op.overload_method() {
                                self.method_proto(type_name, method_name)
                                    .map(|proto| (proto, RetTransform::None))
                            } else if let Some(negate) = op.equatable_negation() {
                                let transform = if negate {
                                    RetTransform::Negate
                                } else {
                                    RetTransform::None
                                };
                                self.method_proto(type_name, "eq")
                                    .map(|proto| (proto, transform))
                            } else if let Some(method_name) = op.comparable_method() {
                                self.method_proto(type_name, method_name)
                                    .map(|proto| (proto, RetTransform::Ordering(*op)))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some((proto, transform)) = dispatch
                            && module.protos[proto as usize].num_params == 2
                        {
                            self.push_callee_frame(
                                frames,
                                regs,
                                top,
                                proto,
                                Some(left),
                                &[right],
                                &[],
                                *dst,
                                transform,
                                pc + 1,
                                // An operator/protocol dispatch of fixed arity — no labels reach it.
                                None,
                                *span,
                            )?;
                            continue 'reload;
                        }
                        // Derived structural comparison: `< <= > >=` on an object or enum whose
                        // type `@derive(Comparable)`s (and has no hand-written `compare`) —
                        // field-wise ordering for objects, variant-declaration-index then payload
                        // for enums, computed synchronously (no method to call).
                        if (left.is_object() || left.is_enum())
                            && op.comparable_method().is_some()
                            && (self.comparable_derives.contains(&left.shape().unwrap().name)
                                // The prelude enums that order without a declaration, because
                                // there is nowhere to write one: `?T` and `Result<T, E>` already
                                // order at every other door, and this is the operator's.
                                || noeta_ast::prelude_enum_orders(&left.shape().unwrap().name))
                        {
                            match structural_compare(left, right) {
                                Some(ordering) => {
                                    let satisfied = op
                                        .ordering_satisfies(noeta_ast::ordering_variant(ordering));
                                    set_reg(regs, fbase, *dst, Value::bool(satisfied));
                                    pc += 1;
                                }
                                None => {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "cannot compare {} and {}",
                                            left.type_name(),
                                            right.type_name()
                                        ),
                                    ));
                                }
                            }
                            continue;
                        }
                        // Element-wise array-programming ops: `+`/`-`/`*` on two lists
                        // of the same numeric element type fold into a new list (`~` is concat, so the
                        // operator is free). One shared `noeta-stdlib` kernel with the tree-walker, so
                        // the differential holds; ints wrap at the element width. The result is a fresh
                        // value (owns its reference) — no retain, like `MaskWidth`.
                        if left.is_list()
                            && right.is_list()
                            && let Some(bop) = elem_bin_op(*op)
                        {
                            let v = self.call_list_elementwise(bop, left, right, *span)?;
                            set_reg(regs, fbase, *dst, v);
                            pc += 1;
                        } else {
                            match apply_binary(*op, left, right) {
                                Ok(v) => {
                                    set_reg(regs, fbase, *dst, v);
                                    pc += 1;
                                }
                                Err(e) => return Err(self.error(e.code, *span, e.text)),
                            }
                        }
                    }
                    Op::RequireBool {
                        reg,
                        side,
                        op,
                        span,
                    } => {
                        let v = regs[fbase + *reg as usize];
                        if v.as_bool().is_none() {
                            let where_ = match side {
                                BoolSide::Left => "left",
                                BoolSide::Right => "right",
                            };
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`{}` expects a bool on the {where_}, found {}",
                                    op.symbol(),
                                    v.type_name()
                                ),
                            ));
                        }
                        pc += 1;
                    }
                    Op::RequireCondBool { reg, span } => {
                        let v = regs[fbase + *reg as usize];
                        if v.as_bool().is_none() {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("`if` condition must be a bool, found {}", v.type_name()),
                            ));
                        }
                        pc += 1;
                    }
                    Op::Jump { target } => {
                        osr_backedge!(*target);
                        pc = *target as usize;
                    }
                    Op::JumpIfTrue { reg, target } => {
                        if regs[fbase + *reg as usize].as_bool() == Some(true) {
                            osr_backedge!(*target);
                            pc = *target as usize;
                        } else {
                            pc += 1;
                        }
                    }
                    Op::JumpIfFalse { reg, target } => {
                        if regs[fbase + *reg as usize].as_bool() == Some(false) {
                            osr_backedge!(*target);
                            pc = *target as usize;
                        } else {
                            pc += 1;
                        }
                    }
                    Op::CondBranch { reg, target, span } => {
                        // Fused bool-check + false-branch (P-VMT-CBR): identical to the
                        // `RequireCondBool` + `JumpIfFalse` pair it replaces.
                        let v = regs[fbase + *reg as usize];
                        match v.as_bool() {
                            Some(false) => {
                                osr_backedge!(*target);
                                pc = *target as usize;
                            }
                            Some(true) => pc += 1,
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`if` condition must be a bool, found {}",
                                        v.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::Echo { reg } => {
                        let text = regs[fbase + *reg as usize].display();
                        self.emit_stdout_line(&text);
                        pc += 1;
                    }
                    // A side-table door inside a generic body: splice its hint against this
                    // frame's render slots and leave the answer where the door that follows reads it
                    // by span. Emitted only there, so an ordinary site never meets one.
                    Op::ResolveHint { span, door, slots } => {
                        self.note_hint_slots(span, *door, slots, regs, fbase);
                        pc += 1;
                        continue;
                    }
                    Op::JsonStringify { dst, src, hint } => {
                        // A JSON door whose value carries an unsigned 64-bit integer: deep-marshal
                        // it and run the one hinted walk, so the erased words reach the wire
                        // unsigned. Byte-identical to the tree-walker, which marshals its own value
                        // into the same neutral tree and runs the same walk.
                        let v = regs[fbase + *src as usize];
                        let resolved = self.resolve_hint_operand(
                            hint,
                            regs,
                            fbase,
                            noeta_stdlib::HintDoor::Json,
                        );
                        let json =
                            noeta_ast::json_stringify(&v.to_native_deep(), resolved.as_deref());
                        set_reg(regs, fbase, *dst, Value::string(&json));
                        pc += 1;
                        continue;
                    }
                    Op::Stringify {
                        dst,
                        src,
                        span,
                        hint,
                    } => {
                        let v = regs[fbase + *src as usize];
                        // A hinted site renders here and now: the hint says the value's static type
                        // holds an unsigned 64-bit integer, whose erased word `display` would read
                        // as signed. The rendered string is what the consuming `Echo`/`BuildString`
                        // then displays (a string displays as itself).
                        // …and where the splice leaves NO hint, the door is an ordinary display
                        // door and falls through to the code below, `Display` dispatch included.
                        // That is what the outermost `Display` exemption means: a concrete-typed
                        // door at such a type records no hint at all and keeps its dispatch, so a
                        // door naming a parameter instantiated at that type must arrive at the same
                        // place — and an instantiation nothing could name must too.
                        if let Some(hint) = hint
                            && let Some(hint) = self
                                .resolve_hint_operand(
                                    hint,
                                    regs,
                                    fbase,
                                    noeta_stdlib::HintDoor::Display,
                                )
                                .as_deref()
                        {
                            let text = v.display_hinted(hint);
                            set_reg(regs, fbase, *dst, Value::from_string(text.into()));
                            pc += 1;
                            continue;
                        }
                        // A user object or enum value lights up the `Display` trait: render it via its
                        // `to_string` method (which runs bytecode, so it is pushed as a call frame). The
                        // method table is keyed by the value's shape name, identical for both kinds.
                        // Matches the tree-walker's `display_value`.
                        if (v.is_object() || v.is_enum())
                            && let Some(proto) =
                                self.method_proto(&v.shape().unwrap().name, "to_string")
                        {
                            let callee_chunk = &module.protos[proto as usize];
                            if callee_chunk.num_params != 1 {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this method takes {} argument(s) but 0 were supplied",
                                        callee_chunk.num_params - 1
                                    ),
                                ));
                            }
                            self.push_callee_frame(
                                frames,
                                regs,
                                top,
                                proto,
                                Some(v),
                                &[],
                                &[],
                                *dst,
                                RetTransform::None,
                                pc + 1,
                                // An operator/protocol dispatch of fixed arity — no labels reach it.
                                None,
                                *span,
                            )?;
                            continue 'reload;
                        }
                        // Identity for every other value: the consuming `Echo`/`Concat` stringifies
                        // it via `display`.
                        let passed = stringify_passthrough(v);
                        set_reg(regs, fbase, *dst, passed);
                        pc += 1;
                    }
                    Op::BuildString { dst, parts } => {
                        let built = build_string(parts, &chunk.consts, regs, fbase);
                        set_reg(regs, fbase, *dst, built);
                        pc += 1;
                    }
                    Op::Call {
                        dst,
                        callee,
                        args,
                        type_args,
                        span,
                        supplied,
                    } => {
                        let callee_val = regs[fbase + *callee as usize];
                        // Shared closure-call setup (also used by the JIT's `jit_call` helper): pushes
                        // the callee frame (→ `continue 'reload`) or completes a first-class-builtin
                        // call synchronously (→ advance to `pc + 1`).
                        if self.setup_closure_call(
                            frames,
                            regs,
                            top,
                            fbase,
                            *dst,
                            callee_val,
                            args,
                            type_args.regs(),
                            *span,
                            pc + 1,
                            noeta_bytecode::supplied_of(*supplied),
                        )? {
                            continue 'reload;
                        }
                        pc += 1;
                    }
                    Op::CallGlobal {
                        dst,
                        global,
                        args,
                        type_args,
                        span,
                        supplied,
                    } => {
                        // A statically-known top-level `fn`: read the callee straight from its
                        // global slot. No retain — the slot owns the reference for the whole call,
                        // so there is no matching release either; net refcount-neutral, exactly as
                        // `LoadGlobal` (retain) balanced by the register overwrite (release) would be.
                        let callee_val = self.persist.globals[global.0 as usize];
                        if callee_val.is_unbound() {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        if self.setup_closure_call(
                            frames,
                            regs,
                            top,
                            fbase,
                            *dst,
                            callee_val,
                            args,
                            type_args.regs(),
                            *span,
                            pc + 1,
                            noeta_bytecode::supplied_of(*supplied),
                        )? {
                            continue 'reload;
                        }
                        pc += 1;
                    }
                    Op::Return { src } => {
                        let raw = regs[fbase + *src as usize];
                        match self.do_return(frames, regs, raw) {
                            // The bottom frame returned: hand the value to `run`'s caller.
                            Some(v) => return Ok(v),
                            // Transferred to a caller — re-derive its window.
                            None => continue 'reload,
                        }
                    }
                    Op::Halt => {
                        let finished = frames.pop().unwrap();
                        let n = module.protos[finished.proto as usize].num_registers as usize;
                        for i in 0..n {
                            release(regs[finished.base + i]);
                        }
                        for u in &finished.upvalues {
                            release(*u);
                        }
                        regs.truncate(finished.base);
                        match frames.last() {
                            // A non-bottom frame falling off the end implicitly returns unit.
                            Some(caller) => {
                                set_reg(regs, caller.base, finished.ret_dst, Value::unit())
                            }
                            // The bottom frame halted: the program (or re-entrant call) ends.
                            None => return Ok(Value::unit()),
                        }
                        // Control returns to the caller (or a re-entry frame) — re-derive its window.
                        continue 'reload;
                    }
                    // --- The cold arms (outlined; perf/outline-cold) ------------------------------
                    //
                    // Every op below is RARE — reflection, tier/typed native calls, coroutine and
                    // isolate ops, aggregate construction, the packed-width helpers, `?`, `panic`.
                    // Their bodies live in [`Vm::cold_op`] (`#[cold] #[inline(never)]`), moved
                    // VERBATIM: same opcode set, same semantics, same order. Nothing about the
                    // program changes — what changes is that their live state no longer takes part
                    // in THIS function's register allocation. That is the whole point: a match arm
                    // that needs many simultaneous values raises the spill/reload traffic every
                    // *other* arm pays on entry and exit, so ~90 rare arms were taxing the dozen hot
                    // ones (arithmetic, compare, jump, move, load/store) on every instruction.
                    //
                    // Listing the patterns explicitly rather than `_` keeps the exhaustiveness check
                    // here: a new `Op` still fails to compile in the dispatch loop, which is where
                    // the decision "is this hot?" belongs.
                    Op::MakeClosure { .. }
                    | Op::LoadNativeFn { .. }
                    | Op::BindMethod { .. }
                    | Op::MakeList { .. }
                    | Op::PackedListNew { .. }
                    | Op::FromBytes { .. }
                    | Op::TypedModuleCall { .. }
                    | Op::TypedMethodCall { .. }
                    | Op::DecodeTyped { .. }
                    | Op::TraitMethod { .. }
                    | Op::PackedListPush { .. }
                    | Op::MakeTuple { .. }
                    | Op::TupleIndex { .. }
                    | Op::MakeRange { .. }
                    | Op::MakeMap { .. }
                    | Op::RequireMapKey { .. }
                    | Op::IterSnapshot { .. }
                    | Op::MakeStruct { .. }
                    | Op::MakeStructInPlace { .. }
                    | Op::MakeOpaque { .. }
                    | Op::EnumFromStr { .. }
                    | Op::Panic { .. }
                    | Op::TryUnwrap { .. }
                    | Op::MakeGen { .. }
                    | Op::MakeFuture { .. }
                    | Op::RunFuture { .. }
                    | Op::PollFuture { .. }
                    | Op::LoadPending { .. }
                    | Op::ScopeBegin
                    | Op::ScopeBeginValue { .. }
                    | Op::ScopeReady { .. }
                    | Op::Spawn { .. }
                    | Op::SpawnIsolate { .. }
                    | Op::ScopeEnd { .. }
                    | Op::ScopeEndAt { .. }
                    | Op::MakeChannel { .. }
                    | Op::AttributesOf { .. }
                    | Op::RolesOf { .. }
                    | Op::ParamsOf { .. }
                    | Op::ReturnsOf { .. }
                    | Op::FieldSpecsOf { .. }
                    | Op::VariantsOf { .. }
                    | Op::Construct { .. }
                    | Op::TypeOf { .. }
                    | Op::TypeArgName { .. }
                    | Op::TypeSlotName { .. }
                    | Op::SelfRenderSlot { .. }
                    | Op::ComposeTypeArg { .. }
                    | Op::FieldsOf { .. }
                    | Op::TraitsOf { .. }
                    | Op::Retag { .. }
                    | Op::RetagDynamic { .. }
                    | Op::TypeOfStatic { .. }
                    | Op::TypeValue { .. }
                    | Op::Invoke { .. }
                    | Op::MaskWidth { .. }
                    | Op::WideInt { .. }
                    | Op::WidthIntMethod { .. }
                    | Op::Raise { .. } => {
                        match self.cold_op(op, frames, regs, module, chunk, fbase, top, pc)? {
                            ColdStep::Next(next) => pc = next,
                            ColdStep::Reload => continue 'reload,
                            ColdStep::Done(v) => return Ok(v),
                        }
                    }
                }
            }
        }
    }

    /// The four **receiver-dispatching** `Op::CallMethod` routes — native module, extern value,
    /// object, enum — outlined out of the dispatch loop (perf/outline-cold); see the call site.
    ///
    /// Each of these resolves a method through a table and, for the user-method cases, pushes a
    /// callee frame. They are cold relative to the built-in chain (`xs.len()`, string methods, map
    /// reads) that every interpreted loop actually runs, and they were more than three quarters of
    /// the arm — which meant the arm's whole operand set stayed live across the dispatch loop and
    /// every other arm paid for it. Moved VERBATIM: same order (native module, extern, object,
    /// enum), same inline caches, same diagnostics. `None` is the fall-through the blocks always
    /// had — the receiver was not one of these four, or the route declined it.
    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn call_method_dispatch(
        &mut self,
        v: Value,
        hk: Option<HeapKind>,
        method: &str,
        dst: &Reg,
        args: &[Reg],
        type_args: &noeta_bytecode::TypeArgs,
        span: &Span,
        cache: &u32,
        supplied: &Option<std::num::NonZero<u64>>,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        caches: &mut [MethodCacheEntry],
        extern_caches: &mut [ExternCacheEntry],
        module: &'m Module,
        fbase: usize,
        top: usize,
        pc: usize,
    ) -> Result<Option<ColdStep>, Abort> {
        // `json.parse(...)` — a Ring 2 native module function call, dispatched before
        // the object/collection paths.
        if hk == Some(HeapKind::NativeModule)
            && let Some(module_name) = v.native_module_name()
        {
            let arg_values = ArgBuf::collect(args, regs, fbase);
            let value =
                self.call_native_module(&module_name, method, arg_values.as_slice(), *span)?;
            set_reg(regs, fbase, *dst, value);
            return Ok(Some(ColdStep::Next(pc + 1)));
        }
        // An extern receiver routes through the per-site cache: a
        // declared arena read inlines to an arena load while its gate is open;
        // ctx methods go straight to their dispatch; anything else falls to the
        // shared by-value chain below.
        if hk == Some(HeapKind::Extern) {
            let ci = *cache as usize;
            // The value's qualified identity (`std.id.Uuid`) — one interned
            // `&'static` literal per type, so the cache key stays a pointer
            // compare and the same string keys ctx dispatch and the read gates.
            let identity = v.with_extern(|e| e.type_identity());
            let route = match extern_caches[ci] {
                Some((key, route)) if key == identity.as_ptr() => route,
                _ => {
                    let route = crate::methods::resolve_extern_route(self.reg(), identity, method);
                    extern_caches[ci] = Some((identity.as_ptr(), route));
                    route
                }
            };
            let is_ctx = match route {
                crate::methods::ExternRoute::FastRead { project } => {
                    if args.is_empty()
                        && (self.persist.ext_closed_gates.is_empty()
                            || !self.persist.ext_closed_gates.contains(&identity))
                    {
                        let retained = v.with_extern(|e| project(e));
                        let value =
                            self.persist.ext_arena[retained as usize].expect("a live arena entry");
                        retain(value);
                        set_reg(regs, fbase, *dst, value);
                        return Ok(Some(ColdStep::Next(pc + 1)));
                    }
                    // Gate closed (or a misuse the dispatch reports): full path.
                    true
                }
                crate::methods::ExternRoute::Ctx => true,
                // The shared by-value chain below owns this (incl. errors).
                crate::methods::ExternRoute::Plain => false,
            };
            if is_ctx {
                let arg_values = ArgBuf::collect(args, regs, fbase);
                let value =
                    self.call_ctx_type_method(identity, v, method, arg_values.as_slice(), *span)?;
                set_reg(regs, fbase, *dst, value);
                return Ok(Some(ColdStep::Next(pc + 1)));
            }
        }
        // An object dispatches to a user method through the type's method table;
        // anything else falls to the built-in `count`/`enumerate` methods.
        if hk == Some(HeapKind::Object) {
            // `o.to_json()` on a type that `@derive(Serialize<Json>)` (so has no hand-written
            // `to_json`) synthesizes a structural JSON string — a pure value
            // computation, so it is produced inline rather than via a call frame. Only a
            // literal `to_json` site reaches here, so the shape clone stays off the common
            // method-call path.
            if method == "to_json" && args.is_empty() {
                let type_name = v.shape().unwrap().name.clone();
                if self.tojson_derives.contains(&type_name) {
                    let json = Value::string(&v.to_json());
                    set_reg(regs, fbase, *dst, json);
                    return Ok(Some(ColdStep::Next(pc + 1)));
                }
            }
            // Inline cache: a hit (the receiver's shape pointer matches the cached one)
            // gives the resolved prototype directly, skipping the `(type, method)` hashmap
            // lookup and its two `String` clones. The hit check avoids bumping the shape
            // refcount (raw pointer compare); only a miss clones the shape into the cache.
            let ci = *cache as usize;
            let shape_ptr = v.object_shape_ptr();
            let hit = match &caches[ci] {
                Some((cs, p)) if Some(std::ptr::from_ref::<Shape>(cs)) == shape_ptr => Some(*p),
                _ => None,
            };
            let proto = match hit {
                Some(proto) => proto,
                None => {
                    let shape = v.shape().unwrap();
                    let Some(proto) = self.method_proto(&shape.name, method) else {
                        // A native class's instance method (native-extensibility S3
                        // / Pass 2a): no hoisted proto by this name, but the shape
                        // names a registered native class that declares it — route
                        // to the class's native `dispatch` (the Object-arm twin of
                        // the extern-method seam). A user class always resolves
                        // through the proto table above, so only a genuine native
                        // class reaches here. Left uncached like the field path.
                        if self.reg().find_class_method(&shape.name, method).is_some() {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            let result = self.call_native_class_method(
                                v,
                                method,
                                arg_values.as_slice(),
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, result);
                            return Ok(Some(ColdStep::Next(pc + 1)));
                        }
                        // A method a `@derive`d built-in trait makes callable
                        // (`to_string`/`eq`/`compare`). Ahead of the field
                        // fallback below because it is a METHOD — the same
                        // precedence the checker pins statically, where its
                        // signature is registered. Left uncached like the field
                        // path: the method cache memoizes prototypes, and this
                        // one has none.
                        {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            if let Some(result) =
                                self.derived_builtin_call(v, method, arg_values.as_slice(), *span)
                            {
                                set_reg(regs, fbase, *dst, result?);
                                return Ok(Some(ColdStep::Next(pc + 1)));
                            }
                        }
                        // The runtime member-call fallback (the field-access-then-
                        // call desugar's `dyn` path): no method `method`, but the
                        // shape HAS a field of that name — `obj.f(args)` means
                        // `(obj.f)(args)`, so call the field's value through the
                        // shared closure-call setup (the `Op::Call` machinery).
                        // The same order the checker pins statically (a method
                        // wins, the field is consulted only on a miss) and the
                        // same route the lowered `Field` + `Call` takes — a
                        // non-callable field value raises the indirect-call E0007
                        // ("... is not callable"), identically in both backends.
                        // Left uncached: the method cache memoizes prototypes,
                        // and this dyn-only path re-probes per call.
                        if let Some(callee_val) = shape.slot_of(method).and_then(|s| v.slot_at(s)) {
                            if self.setup_closure_call(
                                frames,
                                regs,
                                top,
                                fbase,
                                *dst,
                                callee_val,
                                args,
                                // A field holding a callable is a first-class
                                // value, so there is no type-argument channel to
                                // carry; a forwarding callee reached this way
                                // aborts in the setup rather than misbinding.
                                &[],
                                *span,
                                pc + 1,
                                // A member call carries no mask yet — named
                                // arguments bind only to top-level `fn`s so far.
                                None,
                            )? {
                                return Ok(Some(ColdStep::Reload));
                            }
                            return Ok(Some(ColdStep::Next(pc + 1)));
                        }
                        // A bare `from` names no single conversion; say which ones exist.
                        let message = self.missing_method_message(&shape.name, method, false);
                        return Err(self.error(DiagnosticCode::UnknownName, *span, message));
                    };
                    caches[ci] = Some((shape, proto));
                    proto
                }
            };
            let callee_chunk = &module.protos[proto as usize];
            // The prototype takes the receiver in register 0 and the user arguments
            // after it, so its declared arity is one more than the supplied args. A
            // method may have trailing defaulted parameters, so the supplied count is a
            // range `[total - defaults, total]` (all less the receiver). A forwarding
            // generic's hidden slots are not value parameters either — they are filled
            // from `type_args`, so they come off the count too.
            let total = callee_chunk.num_params as usize - 1 - callee_chunk.hidden as usize;
            let required = total - callee_chunk.defaults.len();
            if args.len() < required || args.len() > total {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    *span,
                    arity_message("method", required, total, args.len()),
                ));
            }
            let arg_values = ArgBuf::collect(args, regs, fbase);
            let ty_values = ArgBuf::collect(type_args.regs(), regs, fbase);
            self.push_callee_frame(
                frames,
                regs,
                top,
                proto,
                Some(v),
                arg_values.as_slice(),
                ty_values.as_slice(),
                *dst,
                RetTransform::None,
                pc + 1,
                noeta_bytecode::supplied_of(*supplied),
                *span,
            )?;
            return Ok(Some(ColdStep::Reload));
        }
        // An enum value dispatches to a user method (the unified body) through the
        // same type→method table as an object — and through the same per-site
        // inline cache: an enum's `&'static
        // Shape` handle is as stable an identity as an object's, so a hit resolves
        // the prototype with one pointer compare (the object arm's exact hit test;
        // the two kinds share the slot safely because their shapes are distinct).
        // An unknown method falls through to the built-in paths below — never
        // cached, so the fall-through re-probes exactly as before.
        if hk == Some(HeapKind::Enum) {
            let shape = v.shape().unwrap();
            // `e.to_json()` on an enum that `@derive(Serialize<Json>)`s (and has no
            // hand-written `to_json`): the variant rendering, exactly what
            // `json.stringify` produces — the enum twin of the object arm above.
            if method == "to_json" && args.is_empty() && self.tojson_derives.contains(&shape.name) {
                let json = Value::string(&v.to_json());
                set_reg(regs, fbase, *dst, json);
                return Ok(Some(ColdStep::Next(pc + 1)));
            }
            // `color.value()` on a native **backed** enum: a native enum has no
            // user method proto, so its backing constant
            // is resolved from the registry by the value's (short) name + variant —
            // the twin of the tree-walker's `.value()` accessor.
            if method == "value"
                && args.is_empty()
                && let Some(en) = self.reg().resolve_enum(&shape.name)
                && let Some(variant) = shape.variant.as_deref()
                && let Some((_, vdef)) = en.variant(variant)
            {
                let out = match vdef.value {
                    noeta_stdlib::VariantValue::Str(s) => Value::string(s),
                    noeta_stdlib::VariantValue::Int(n) => Value::int(n),
                    noeta_stdlib::VariantValue::None => Value::unit(),
                };
                set_reg(regs, fbase, *dst, out);
                return Ok(Some(ColdStep::Next(pc + 1)));
            }
            let ci = *cache as usize;
            let hit = match &caches[ci] {
                Some((cs, p)) if std::ptr::eq::<Shape>(*cs, shape) => Some(*p),
                _ => None,
            };
            let proto = match hit {
                Some(proto) => Some(proto),
                None => {
                    let resolved = self.method_proto(&shape.name, method);
                    if let Some(proto) = resolved {
                        caches[ci] = Some((shape, proto));
                    }
                    resolved
                }
            };
            if let Some(proto) = proto {
                let callee_chunk = &module.protos[proto as usize];
                let total = callee_chunk.num_params as usize - 1 - callee_chunk.hidden as usize;
                let required = total - callee_chunk.defaults.len();
                if args.len() < required || args.len() > total {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        arity_message("method", required, total, args.len()),
                    ));
                }
                let arg_values = ArgBuf::collect(args, regs, fbase);
                let ty_values = ArgBuf::collect(type_args.regs(), regs, fbase);
                self.push_callee_frame(
                    frames,
                    regs,
                    top,
                    proto,
                    Some(v),
                    arg_values.as_slice(),
                    ty_values.as_slice(),
                    *dst,
                    RetTransform::None,
                    pc + 1,
                    noeta_bytecode::supplied_of(*supplied),
                    *span,
                )?;
                return Ok(Some(ColdStep::Reload));
            }
            // A native enum's instance method: no user proto and not the built-in
            // `value()`/`to_json`, but the shape
            // names a registered native enum that declares it — route to the enum's
            // native `dispatch`, the enum twin of the Object arm's `find_class_method`
            // → `call_native_class_method` fall-through above. Left uncached like the
            // value/field paths.
            if self.reg().find_enum_method(&shape.name, method).is_some() {
                let arg_values = ArgBuf::collect(args, regs, fbase);
                let result =
                    self.call_native_enum_method(v, method, arg_values.as_slice(), *span)?;
                set_reg(regs, fbase, *dst, result);
                return Ok(Some(ColdStep::Next(pc + 1)));
            }
        }
        Ok(None)
    }

    /// The profiler's per-instruction consult (`noeta profile`), outlined out of the dispatch loop
    /// — see the call site. `None` on every non-profile run, so this is never entered there; the
    /// body is the one that used to be inline, verbatim.
    #[cold]
    #[inline(never)]
    fn profile_before_op(
        &mut self,
        module: &'m Module,
        frames: &mut [Frame],
        regs: &[Value],
        top: usize,
        pc: usize,
    ) {
        let strand = self.sched.current_strand;
        if let Some(prof) = self.profiler.as_mut() {
            frames[top].pc = pc;
            let view = DebugView {
                module,
                frames: &frames[..],
                regs,
                globals: &self.persist.globals,
                strand,
            };
            prof.before_op(&view);
        }
    }

    /// The debugger's per-instruction consult (`noeta dap`), outlined out of the dispatch loop —
    /// see the call site. `None` on every non-debug run, so this is never entered there; the body
    /// is the one that used to be inline, verbatim, including holding the debugger *out* of `self`
    /// for the whole pause so a watch expression can re-enter the VM.
    #[cold]
    #[inline(never)]
    fn debug_before_op(
        &mut self,
        module: &'m Module,
        frames: &mut [Frame],
        regs: &mut [Value],
        top: usize,
        proto: usize,
        pc: usize,
    ) -> Result<(), Abort> {
        let strand = self.sched.current_strand;
        frames[top].pc = pc;
        // Hold the debugger *out* of `self` for the whole pause. This frees `&mut self` so a
        // watch expression that calls a function can actually run it (`debug_eval_request`
        // re-enters the VM), and it auto-disarms that nested run's own debug consults —
        // `self.debugger` is `None` while paused, so evaluating `f(x)` never breaks inside
        // `f`. The debugger is restored before we resume normal dispatch.
        let mut dbg = self.debugger.take().unwrap();
        loop {
            let action = {
                let view = DebugView {
                    module,
                    frames: &frames[..],
                    regs: &regs[..],
                    globals: &self.persist.globals,
                    strand,
                };
                dbg.before_op(proto as u32, pc, &view)
            };
            match action {
                DebugAction::Continue => {
                    // The program resumes (continue or a landed step): the observed
                    // state may change before the next stop, so advance the generation
                    // and invalidate every memoized watch result (watch-memoization).
                    self.bump_stop_generation();
                    break;
                }
                DebugAction::Terminate => {
                    self.debugger = Some(dbg);
                    return Err(Abort);
                }
                // A watch/console evaluate that needs the VM (a call). Run it here with
                // `&mut self`, reply, then loop: `before_op` re-enters its wait silently.
                DebugAction::Evaluate(req) => {
                    let DebugEvalRequest {
                        program,
                        text,
                        frame,
                        scope,
                        kind,
                        reply,
                    } = req;
                    // Every evaluate compiles through the adopted session: full
                    // language for a watch/console, and for a hover the same engine
                    // gated to the read-only surface — one evaluator, not two. The
                    // `kind` also drives watch-memoization (cache reuse + generation
                    // bumping) inside `debug_eval_fragment`.
                    let outcome = if self.debug_session.is_some() {
                        self.debug_eval_fragment(
                            &program,
                            frame,
                            &scope,
                            kind,
                            &text,
                            &frames[..],
                            &regs[..],
                        )
                    } else {
                        DebugEvalOutcome::Error(
                            "this debug run has no console session — evaluate needs a \
                                     session launch"
                                .to_string(),
                        )
                    };
                    let _ = reply.send(outcome);
                }
                // A Variables-panel edit: evaluate the replacement value as a
                // console fragment and write the frame's register in place.
                DebugAction::SetVariable(req) => {
                    let DebugSetRequest {
                        name,
                        value,
                        frame,
                        scope,
                        reply,
                    } = req;
                    let outcome = if self.debug_session.is_some() {
                        self.debug_set_variable(
                            &name,
                            &value,
                            frame,
                            &scope,
                            &frames[..],
                            &mut regs[..],
                        )
                    } else {
                        DebugEvalOutcome::Error(
                            "this debug run has no console session — setVariable needs \
                                     a session launch"
                                .to_string(),
                        )
                    };
                    // Refresh the adapter-visible snapshot with the just-written
                    // register BEFORE unblocking the client, so a `variables`/
                    // `stackTrace` racing in behind this response reads the new value
                    // rather than the stale pause-time capture.
                    {
                        let view = DebugView {
                            module,
                            frames: &frames[..],
                            regs: &regs[..],
                            globals: &self.persist.globals,
                            strand,
                        };
                        dbg.after_side_effect(&view);
                    }
                    let _ = reply.send(outcome);
                }
            }
        }
        self.debugger = Some(dbg);
        Ok(())
    }

    /// The **cold half of the op match** (perf/outline-cold): every rare opcode's body, moved
    /// verbatim out of [`Vm::dispatch_inner`].
    ///
    /// WHY THIS EXISTS. `dispatch_inner` is one function holding the whole ~104-arm match, and a
    /// register allocator works over the *whole* function: the more simultaneously-live values ANY
    /// arm needs, the more spill/reload traffic EVERY arm pays around it — including the dozen hot
    /// ones the interpreter's speed is made of (arithmetic, compare, jump, register move,
    /// local/global load-store). Reflection, typed native calls, coroutine/isolate ops, aggregate
    /// construction, the packed-width helpers, `?` and `panic` are none of them rare-but-fat, and
    /// together they were more than half the match's code. Marking them `#[cold]` +
    /// `#[inline(never)]` takes their live state out of the hot loop's allocation problem for the
    /// price of one call on the ops nobody measures.
    ///
    /// NOTHING ELSE CHANGES. Same opcodes, same semantics, same relative order, same diagnostics;
    /// the arms are byte-identical apart from how they spell their two exits (see [`ColdStep`]).
    /// The shared leaf-op seam is untouched: these arms still call the same `#[inline(always)]`
    /// free functions (`make_range_list`, `iter_snapshot_value_hinted`, `make_tuple`,
    /// `tuple_element_retained`, …) that tier-1's `jit_run_leaf_op` calls — one behavior, one
    /// implementation, two callers, exactly as before.
    ///
    /// The arms' exits are spelled as they were in the loop, mechanically re-targeted:
    /// `pc += 1;` falling out of the arm ⟶ [`ColdStep::Next`]; `continue;` ⟶ `break 'arm`
    /// (same meaning: this arm is done, take the `pc` it set); `continue 'reload;` ⟶
    /// [`ColdStep::Reload`]; a bottom-frame `return Ok(v)` ⟶ [`ColdStep::Done`].
    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn cold_op(
        &mut self,
        op: &'m Op,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        module: &'m Module,
        chunk: &'m Chunk,
        fbase: usize,
        top: usize,
        mut pc: usize,
    ) -> Result<ColdStep, Abort> {
        'arm: {
            match op {
                Op::MakeClosure {
                    dst,
                    proto,
                    captures,
                } => {
                    // Gather one cell per capture (from a celled local register, or one of this
                    // frame's own upvalues — forwarding a capture down a level), retaining each
                    // into the new closure, which owns its upvalue cells.
                    let mut upvalues = Vec::with_capacity(captures.len());
                    for capture in captures.iter() {
                        let cell = match capture {
                            CaptureFrom::Local(reg) => regs[fbase + *reg as usize],
                            CaptureFrom::Upvalue(index) => frames[top].upvalues[*index as usize],
                        };
                        retain(cell);
                        upvalues.push(cell);
                    }
                    let v = Value::closure(*proto, upvalues);
                    set_reg(regs, fbase, *dst, v);
                    pc += 1;
                }
                Op::LoadNativeFn { dst, func } => {
                    set_reg(regs, fbase, *dst, Value::native_fn(*func));
                    pc += 1;
                }
                Op::BindMethod { dst, recv, method } => {
                    // A bound method handle (`value.method`, EX.2b): capture one retained
                    // reference to the receiver.
                    let recv_val = regs[fbase + *recv as usize];
                    retain(recv_val);
                    let handle = Value::bound_method(recv_val, module.name(*method));
                    set_reg(regs, fbase, *dst, handle);
                    pc += 1;
                }
                Op::MakeList {
                    dst,
                    items,
                    reflect,
                } => {
                    let mut elements = Vec::with_capacity(items.len());
                    for &r in items.iter() {
                        let v = regs[fbase + r as usize];
                        retain(v);
                        elements.push(v);
                    }
                    let list = Value::list(elements);
                    // Stamp the checker-resolved element type onto the list so `type_of` recovers
                    // it after a `dyn` launder. A cheap `Rc` clone of the shared load-time entry; the
                    // tag lives beside the payload, invisible to value semantics.
                    if let Some(idx) = reflect {
                        list.set_reflect(Some(Rc::clone(&self.persist.type_reprs[*idx as usize])));
                    }
                    set_reg(regs, fbase, *dst, list);
                    pc += 1;
                }
                Op::PackedListNew { dst, schema } => {
                    // Allocate the empty flat buffer the following `PackedListPush` chain fills
                    // (streaming construction).
                    let schema = self.persist.packed_schemas[*schema as usize];
                    let list = Value::packed_list(schema, Vec::new());
                    set_reg(regs, fbase, *dst, list);
                    pc += 1;
                }
                Op::FromBytes {
                    dst,
                    src,
                    schema,
                    validate,
                    span,
                } => {
                    // Deserialize a `bytes` buffer into a flat `List<T>`: wrap the raw
                    // bytes as a packed list of the interned schema — the inverse of `to_bytes`.
                    let blob = regs[fbase + *src as usize];
                    let Some(bytes) = blob.bytes_data() else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`from_bytes` expects a `bytes` value, found {}",
                                blob.type_name()
                            ),
                        ));
                    };
                    let schema = self.persist.packed_schemas[*schema as usize];
                    if schema.byte_size == 0 || bytes.len() % schema.byte_size != 0 {
                        return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    *span,
                    format!(
                        "`from_bytes` buffer of {} bytes is not a whole number of {}-byte elements",
                        bytes.len(),
                        schema.byte_size
                    ),
                ));
                    }
                    let list = Value::packed_list(schema, bytes);
                    // Validation arc: `from_bytes` is an abort door — run each decoded element's
                    // `validate()` (materialized boxed for the re-entry, then consumed) and abort
                    // at `[i]` on the first rejection, consistent with a length/shape mismatch.
                    if *validate {
                        let n = list.list_len().unwrap_or(0);
                        for i in 0..n {
                            let element = list.packed_get(i); // owned (rc 1), consumed below
                            if let Some(message) = self.validate_message(element, *span)? {
                                release(list);
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("from_bytes: [{i}]: {message}"),
                                ));
                            }
                        }
                    }
                    set_reg(regs, fbase, *dst, list);
                    pc += 1;
                }
                Op::TypedModuleCall {
                    dst,
                    module: mod_id,
                    func: func_id,
                    args,
                    recipe,
                    dynamic,
                    span,
                } => {
                    // Resolve the interned module/func names (`module` is the outer loop-local
                    // `&Module`, so bind the op's ids under different names to avoid shadowing it).
                    let mod_name = module.name(*mod_id);
                    let func = module.name(*func_id);
                    // The recipe: baked at the call site, or resolved
                    // per-instantiation through the forwarding fn's hidden slot register — an
                    // index into the module's type-argument table. Mirrors the tree-walker.
                    let dynamic_recipe = dynamic.and_then(|slot| {
                        let idx = regs[fbase + slot as usize].as_int().unwrap_or(-1);
                        module
                            .type_args
                            .get(idx.max(0) as usize)
                            .and_then(|e| e.recipe.clone().map(Box::new))
                    });
                    let recipe = match dynamic {
                        Some(_) => &dynamic_recipe,
                        None => recipe,
                    };
                    // The recipe is required; its absence was already reported by the checker.
                    let Some(recipe) = recipe else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`{mod_name}.{func}::<T>(...)` has no resolved result type"),
                        ));
                    };
                    // Route through the registry's call-site-typed seam: the module's
                    // `typed_dispatch`, threaded the recipe, builds the whole `NativeOut` tree
                    // (already carrying its declared wrapper — `Ok`/`Err` for a `Result` shape,
                    // `Some`/`None` for `Option`), which materializes to a value of `T`. No
                    // function name is special-cased — `json.parse`/`try_parse` are registered
                    // like any extension's call-site-typed functions.
                    let reg = self.reg();
                    let Some(ext_mod) = reg
                        .find_module(mod_name)
                        .filter(|_| reg.find_typed_function(mod_name, func).is_some())
                    else {
                        return Err(self.error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!(
                            "`{mod_name}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    ));
                    };
                    let Some(typed_dispatch) = ext_mod.typed_dispatch else {
                        return Err(self.error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!(
                            "`{mod_name}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    ));
                    };
                    // A reflective module (`json`) marshals its arguments deeply; every other
                    // uses the cheap shallow projection — the same decision the plain module
                    // dispatch makes.
                    let nargs: Vec<noeta_stdlib::NativeValue> = args
                        .iter()
                        .map(|r| {
                            let v = regs[fbase + *r as usize];
                            if ext_mod.deep_marshal {
                                v.to_native_deep()
                            } else {
                                marshal_native_arg(v, reg)
                            }
                        })
                        .collect();
                    match typed_dispatch(func, &mut *self.persist.host, &nargs, recipe) {
                        Ok(out) => {
                            // The aborting door (`json.parse::<T>`): a validation rejection that
                            // reaches the top (not recovered by a `Result` wrapper) aborts with
                            // the same path-precise message the abort door uses for shape
                            // failures. `json.try_parse::<T>` wraps its result in `Ok`/`Err`, so
                            // its rejections are recovered inside `materialize_recipe` and never
                            // reach here.
                            let mut path = String::new();
                            match self.materialize_recipe(out, &mut path, *span)? {
                                MatOut::Value(v) => set_reg(regs, fbase, *dst, v),
                                MatOut::Rejected(e) => {
                                    return Err(self.std_dispatch_error(e.into_std_error(), *span));
                                }
                            }
                        }
                        Err(error) => return Err(self.std_dispatch_error(error, *span)),
                    }
                    pc += 1;
                }
                Op::TypedMethodCall {
                    dst,
                    recv,
                    method: method_id,
                    args,
                    recipe,
                    dynamic,
                    span,
                } => {
                    // The extern-METHOD twin of `TypedModuleCall` above, step for
                    // step — the only differences are that the receiver's own runtime identity
                    // selects the type (no module lookup) and the dispatch takes the receiver.
                    // Mirrors the tree-walker, so the two backends agree by construction.
                    let method = module.name(*method_id);
                    let dynamic_recipe = dynamic.and_then(|slot| {
                        let idx = regs[fbase + slot as usize].as_int().unwrap_or(-1);
                        module
                            .type_args
                            .get(idx.max(0) as usize)
                            .and_then(|e| e.recipe.clone().map(Box::new))
                    });
                    let recipe = match dynamic {
                        Some(_) => &dynamic_recipe,
                        None => recipe,
                    };
                    let Some(recipe) = recipe else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`{method}::<T>(...)` has no resolved result type"),
                        ));
                    };
                    let receiver = regs[fbase + *recv as usize];
                    if receiver.heap_kind() != Some(HeapKind::Extern) {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`{method}::<T>(...)` needs a native receiver"),
                        ));
                    }
                    let deep = receiver.with_extern(|e| {
                        self.reg()
                            .find_type_qualified(e.type_identity())
                            .is_some_and(|t| t.deep_marshal)
                    });
                    let reg = self.reg();
                    let nargs: Vec<noeta_stdlib::NativeValue> = args
                        .iter()
                        .map(|r| {
                            let v = regs[fbase + *r as usize];
                            if deep {
                                v.to_native_deep()
                            } else {
                                marshal_native_arg(v, reg)
                            }
                        })
                        .collect();
                    let host = &mut *self.persist.host;
                    let out = receiver.with_extern_mut(|e| {
                        reg.dispatch_typed_method(e, method, host, &nargs, recipe)
                    });
                    match out {
                        Ok(out) => {
                            let mut path = String::new();
                            match self.materialize_recipe(out, &mut path, *span)? {
                                MatOut::Value(v) => set_reg(regs, fbase, *dst, v),
                                MatOut::Rejected(e) => {
                                    return Err(self.std_dispatch_error(e.into_std_error(), *span));
                                }
                            }
                        }
                        Err(error) => return Err(self.std_dispatch_error(error, *span)),
                    }
                    pc += 1;
                }
                Op::DecodeTyped {
                    dst,
                    name,
                    text,
                    ok_shape,
                    err_shape,
                    span,
                } => {
                    // The router-facing runtime decode. Fully recoverable: an unknown
                    // type name, a non-string operand, or a malformed body all land as
                    // `Result.Err` wrapping a path-carrying `JsonError` (the same error story
                    // as `json.try_parse::<T>`). Mirrors the recoverable `try_parse` branch
                    // above, but the recipe is looked up by runtime type name rather than
                    // baked at the call site.
                    let err = |vm: &Self, error: noeta_stdlib::json::JsonError| {
                        Value::enum_value(
                            vm.persist.shapes[*err_shape as usize],
                            vec![Value::extern_value(noeta_stdlib::ExternBox::new(error))],
                        )
                    };
                    let name_val = regs[fbase + *name as usize].as_string();
                    let text_val = regs[fbase + *text as usize].as_string();
                    let (Some(type_name), Some(text)) = (name_val, text_val) else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "`json.decode_typed` expects two `string` arguments".to_string(),
                        ));
                    };
                    // Clone the recipe out of `self` first: materializing it re-enters the VM
                    // (`&mut self`) to run any `Validate::validate`, which cannot coexist with a
                    // borrow of `self.deserialize_recipes`.
                    let recipe = self.deserialize_recipes.get(&type_name).cloned();
                    let value = match recipe {
                        None => err(
                            self,
                            noeta_stdlib::json::JsonError::unknown_type(&type_name),
                        ),
                        Some(recipe) => {
                            match noeta_stdlib::json::try_parse_typed(&text, &recipe) {
                                // The recoverable router door: a validation rejection is
                                // threaded into the `Result.Err(JsonError)`, exactly like a
                                // shape failure.
                                Ok(out) => {
                                    let mut path = String::new();
                                    match self.materialize_recipe(out, &mut path, *span)? {
                                        MatOut::Value(v) => Value::enum_value(
                                            self.persist.shapes[*ok_shape as usize],
                                            vec![v],
                                        ),
                                        MatOut::Rejected(e) => err(self, e),
                                    }
                                }
                                Err(error) => err(self, error),
                            }
                        }
                    };
                    set_reg(regs, fbase, *dst, value);
                    pc += 1;
                }
                Op::TraitMethod {
                    dst,
                    recv,
                    trait_name: trait_id,
                    method: method_id,
                    args,
                    span,
                } => {
                    // A trait default-body call: the
                    // (trait, method) route was baked at compile time — straight to the trait's
                    // shared ctx dispatch, receiver as slot 0. Values are copied out of the
                    // registers first (borrowed by the ctx seed; the seam owns the refcount).
                    let trait_qname = module.name(*trait_id);
                    let method_name = module.name(*method_id);
                    let recv_value = regs[fbase + *recv as usize];
                    let mut arg_values = Vec::with_capacity(args.len());
                    for r in args.iter() {
                        arg_values.push(regs[fbase + *r as usize]);
                    }
                    let result = self.call_trait_method(
                        trait_qname,
                        method_name,
                        recv_value,
                        &arg_values,
                        *span,
                    )?;
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::PackedListPush {
                    dst, list, value, ..
                } => {
                    let acc = regs[fbase + *list as usize];
                    let element = regs[fbase + *value as usize];
                    // `list` is the streaming accumulator — a uniquely-owned temp. Clear its register
                    // to `unit` *without* releasing (a direct overwrite, like `ConcatInPlace`), so the
                    // single owning reference transfers into `result` and a `dst == list` store is
                    // safe. `value` is left in its register for the compiler-emitted `Drop` to free.
                    regs[fbase + *list as usize] = Value::unit();
                    let result = if acc.is_packed_list() {
                        if acc.packed_push(element) {
                            // Element primitives copied into the buffer (not retained) — the buffer
                            // extended in place; the `Drop` of `value` frees the element object.
                            acc
                        } else {
                            // Defensive demote (a checked `@packed` type never mismatches): materialize
                            // the packed buffer to an owned boxed list, release the packed accumulator,
                            // then push the (retained) element so the boxed list owns one reference.
                            let boxed = acc.realize_list();
                            release(acc);
                            retain(element);
                            boxed.list_push(element);
                            boxed
                        }
                    } else {
                        // Already boxed (a prior demote): push the retained element in place.
                        retain(element);
                        acc.list_push(element);
                        acc
                    };
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::MakeTuple { dst, items } => {
                    let tuple = make_tuple(items, regs, fbase);
                    set_reg(regs, fbase, *dst, tuple);
                    pc += 1;
                }
                Op::TupleIndex {
                    dst,
                    receiver,
                    index,
                    span,
                } => {
                    let v = regs[fbase + *receiver as usize];
                    let Some(element) = tuple_element_retained(v, *index as usize) else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "tuple index `{index}` is out of range for {}",
                                v.type_name()
                            ),
                        ));
                    };
                    set_reg(regs, fbase, *dst, element);
                    pc += 1;
                }
                Op::MakeRange {
                    dst,
                    start,
                    end,
                    inclusive,
                    span,
                } => {
                    let lo = regs[fbase + *start as usize];
                    let hi = regs[fbase + *end as usize];
                    match make_range_list(lo, hi, *inclusive) {
                        Some(list) => {
                            set_reg(regs, fbase, *dst, list);
                            pc += 1;
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "range bounds must be ints, found {} and {}",
                                    lo.type_name(),
                                    hi.type_name()
                                ),
                            ));
                        }
                    }
                }
                Op::MakeMap {
                    dst,
                    entries,
                    reflect,
                } => {
                    let mut map: Vec<(noeta_stdlib::MapKey, Value)> =
                        Vec::with_capacity(entries.len());
                    for (key_reg, value_reg) in entries.iter() {
                        // Validated by the preceding `RequireMapKey`: a string (its P-SSO
                        // compact clone), a key-capable packed struct (a content snapshot,
                        // P-PKEY), or a key-capable extern value (a boxed snapshot).
                        let key_value = regs[fbase + *key_reg as usize];
                        let key = match key_value.as_compact_string() {
                            Some(s) => noeta_stdlib::MapKey::Str(s),
                            None => match key_value.as_int() {
                                Some(i) => noeta_stdlib::MapKey::Int(i),
                                None => match key_value.packed_map_key() {
                                    Some(k) => k,
                                    None => key_value.with_extern(|e| {
                                        noeta_stdlib::MapKey::Extern(noeta_stdlib::ExternBox(
                                            e.clone_box(),
                                        ))
                                    }),
                                },
                            },
                        };
                        let value = regs[fbase + *value_reg as usize];
                        retain(value);
                        // A duplicate key keeps the later value (tree-walker `BTreeMap` semantics); the
                        // displaced value loses its owner, so release it.
                        if let Some(pos) = map.iter().position(|(k, _)| *k == key) {
                            let (_, old) = map.remove(pos);
                            release(old);
                        }
                        map.push((key, value));
                    }
                    let map = Value::map_keyed(map);
                    // Stamp the checker-resolved `Map(K, V)` type onto the map so `type_of`
                    // recovers it after a `dyn` launder — the same node-tag path `MakeList` uses.
                    if let Some(idx) = reflect {
                        map.set_reflect(Some(Rc::clone(&self.persist.type_reprs[*idx as usize])));
                    }
                    set_reg(regs, fbase, *dst, map);
                    pc += 1;
                }
                Op::RequireMapKey { reg, span } => {
                    let v = regs[fbase + *reg as usize];
                    let ok = v.is_string()
                    // Ints key maps (`float` stays excluded — NaN).
                    || v.as_int().is_some()
                    || (v.is_extern()
                        && v.with_extern(noeta_stdlib::map_key::extern_key_capable))
                    // A key-capable `@packed` struct keys a map by content.
                    || v.shape().is_some_and(|s| s.key_capable);
                    if !ok {
                        let error = noeta_stdlib::map_key::map_key_error(v.type_name());
                        return Err(self.error(DiagnosticCode::TypeMismatch, *span, error.message));
                    }
                    pc += 1;
                }
                Op::IterSnapshot { dst, src, span } => {
                    let v = regs[fbase + *src as usize];
                    let order = self.order_hint(span);
                    // A user object lights up the `Iterable` trait: `for x in o` iterates the list
                    // its `iter` method returns. The method runs bytecode, so it is pushed as a
                    // call frame; its returned value becomes the snapshot (the following `ListLen`
                    // raises E0007 if it was not a list). Matches the tree-walker's `exec_for`.
                    if v.is_object()
                        && let Some(proto) = self.method_proto(&v.shape().unwrap().name, "iter")
                    {
                        let callee_chunk = &module.protos[proto as usize];
                        if callee_chunk.num_params != 1 {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "this method takes {} argument(s) but 0 were supplied",
                                    callee_chunk.num_params - 1
                                ),
                            ));
                        }
                        self.push_callee_frame(
                            frames,
                            regs,
                            top,
                            proto,
                            Some(v),
                            &[],
                            // A protocol method is reached by its fixed name, so there is no
                            // instantiation to carry; the guard aborts rather than misbind if
                            // one is ever declared generic.
                            &[],
                            *dst,
                            RetTransform::None,
                            pc + 1,
                            // A fixed-arity protocol call — no labels reach it.
                            None,
                            *span,
                        )?;
                        return Ok(ColdStep::Reload);
                    }
                    // The member-handle iterator (coroutines Track-I trigger): a user object
                    // with no `iter` but a callable `next` member — a method, or a
                    // closure-valued field — drains into a materialized snapshot list,
                    // exactly like the tree-walker.
                    if v.is_object() && self.has_user_next(v) {
                        let list = self.drain_next_object(v, *span)?;
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                        break 'arm;
                    }
                    // Snapshot the elements to iterate: a packed list materializes into an owned
                    // boxed snapshot (so `ListLen`/`ListGet` never see the flat form); a list's
                    // elements, a set's canonical elements, or a map's values in sorted-key order
                    // are each retained so the loop owns them independently.
                    let Some(snapshot) = iter_snapshot_value_hinted(v, order) else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot iterate over {}", v.type_name()),
                        ));
                    };
                    set_reg(regs, fbase, *dst, snapshot);
                    pc += 1;
                }
                Op::MakeStruct {
                    dst,
                    shape,
                    named,
                    spread,
                    reflect,
                    span,
                } => {
                    let shape = self.persist.shapes[*shape as usize];
                    let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                    // `...base` fills declared slots the base provides; named initializers then
                    // override. A slot left unset by both is a missing-field error (E0009).
                    if let Some(base_reg) = spread {
                        let base = regs[fbase + *base_reg as usize];
                        for (i, field) in shape.fields.iter().enumerate() {
                            if let Some(value) = base.field(field) {
                                retain(value);
                                slots[i] = Some(value);
                            }
                        }
                    }
                    for (slot, reg) in named.iter() {
                        let value = regs[fbase + *reg as usize];
                        retain(value);
                        if let Some(old) = slots[*slot as usize].replace(value) {
                            release(old);
                        }
                    }
                    // A slot still unset after spread + named is filled from its field default,
                    // run in global scope (empty upvalues — a default resolves globals
                    // only). A slot with neither a value nor a default violates the
                    // full-initialization guarantee (E0009).
                    let mut missing: Vec<String> = Vec::new();
                    for i in 0..shape.fields.len() {
                        if slots[i].is_some() {
                            continue;
                        }
                        let field = shape.fields[i].clone();
                        if let Some(&proto) = self
                            .field_defaults
                            .get(&(shape.name.clone(), field.clone()))
                        {
                            match self.run_thunk(proto, &[]) {
                                Ok(value) => slots[i] = Some(value),
                                Err(abort) => {
                                    for slot in slots.into_iter().flatten() {
                                        release(slot);
                                    }
                                    return Err(abort);
                                }
                            }
                        } else {
                            missing.push(field);
                        }
                    }
                    if !missing.is_empty() {
                        for slot in slots.into_iter().flatten() {
                            release(slot);
                        }
                        let list = missing
                            .iter()
                            .map(|name| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(self.error(
                            DiagnosticCode::MissingField,
                            *span,
                            format!(
                                "missing field(s) {list} in `{}` literal — every field must be set",
                                shape.name
                            ),
                        ));
                    }
                    let slots = slots.into_iter().map(Option::unwrap).collect();
                    let object = Value::object(shape, slots);
                    // Stamp the reflected type onto a generic instantiation so `type_of` recovers
                    // its type arguments after a `dyn` launder. The object's type is invariant under
                    // field mutation, so — unlike the collection tags — it is never cleared.
                    if let Some(idx) = reflect {
                        object
                            .set_reflect(Some(Rc::clone(&self.persist.type_reprs[*idx as usize])));
                    }
                    set_reg(regs, fbase, *dst, object);
                    pc += 1;
                }
                Op::MakeStructInPlace {
                    dst,
                    shape,
                    named,
                    base,
                    check,
                    reflect,
                    span,
                } => {
                    let shape = self.persist.shapes[*shape as usize];
                    // The base is consumed: take its single reference out of the register without
                    // releasing (a direct overwrite, mirroring `ConcatInPlace`), so the refcount
                    // below still counts the accumulator's reference and a `dst == base` store is
                    // safe (the old occupant is now `unit`).
                    let base_val = regs[fbase + *base as usize];
                    regs[fbase + *base as usize] = Value::unit();
                    let same_shape =
                        base_val.object_shape_ptr() == Some(std::ptr::from_ref::<Shape>(shape));
                    let reuse = match check {
                        ReuseCheck::Static => {
                            // The linearity analysis proved sole ownership, so the **refcount** check
                            // is elided — this is the compile-time-hoisted uniqueness path. The debug
                            // assertion documents (and, in debug builds, guards) that invariant; a
                            // failure means the analysis is wrong. The shape is still guarded (a
                            // well-typed self-update always matches, but a mismatch must fall back to
                            // copy rather than corrupt the object at the wrong slot layout).
                            debug_assert!(
                                base_val.is_uniquely_owned(),
                                "static record reuse requires a uniquely-owned base"
                            );
                            same_shape
                        }
                        ReuseCheck::Runtime => same_shape && base_val.is_uniquely_owned(),
                    };
                    if reuse {
                        // Reuse the allocation: overwrite only the changed slots. Every unchanged
                        // field keeps base's reference, which transfers into the result — base *is*
                        // the result. The displaced old field value is routed through `release_value`
                        // (not a plain free) so its `destruct` fires at the right time — matching the
                        // copy-and-destroy baseline, which would destroy the old base and its fields
                        // (spec §4/§5). The reuse pass guarantees `base`'s own type has no destructor,
                        // so reuse never skips a container destructor.
                        for (slot, reg) in named.iter() {
                            let v = regs[fbase + *reg as usize];
                            let old = base_val.replace_slot(*slot as usize, v);
                            self.release_value(old);
                        }
                        // Reuse keeps the base node's existing reflected type: a self-update
                        // rebuilds a value of the same (generic) type, so the base's tag already carries
                        // it — matching the tree-walker's reuse path, which keeps the accumulator's tag.
                        set_reg(regs, fbase, *dst, base_val);
                        pc += 1;
                    } else {
                        // Aliased or a different shape: build a fresh object exactly like
                        // `MakeStruct` (spreading base's fields), then release the consumed base.
                        let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                        for (i, field) in shape.fields.iter().enumerate() {
                            if let Some(value) = base_val.field(field) {
                                retain(value);
                                slots[i] = Some(value);
                            }
                        }
                        for (slot, reg) in named.iter() {
                            let value = regs[fbase + *reg as usize];
                            retain(value);
                            if let Some(old) = slots[*slot as usize].replace(value) {
                                release(old);
                            }
                        }
                        let missing: Vec<&str> = shape
                            .fields
                            .iter()
                            .zip(&slots)
                            .filter(|(_, slot)| slot.is_none())
                            .map(|(name, _)| name.as_str())
                            .collect();
                        if !missing.is_empty() {
                            for slot in slots.into_iter().flatten() {
                                release(slot);
                            }
                            release(base_val);
                            let list = missing
                                .iter()
                                .map(|name| format!("`{name}`"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(self.error(
                        DiagnosticCode::MissingField,
                        *span,
                        format!(
                            "missing field(s) {list} in `{}` literal — every field must be set",
                            shape.name
                        ),
                    ));
                        }
                        let slots = slots.into_iter().map(Option::unwrap).collect();
                        release(base_val);
                        let object = Value::object(shape, slots);
                        if let Some(idx) = reflect {
                            object.set_reflect(Some(Rc::clone(
                                &self.persist.type_reprs[*idx as usize],
                            )));
                        }
                        set_reg(regs, fbase, *dst, object);
                        pc += 1;
                    }
                }
                Op::MakeOpaque {
                    dst,
                    type_name,
                    keys,
                    spread,
                } => {
                    // An opaque object's shape is built from its (spread ∪ named) keys in sorted
                    // order, so its display matches the tree-walker's `BTreeMap` field bag.
                    let mut bag: BTreeMap<String, Value> = BTreeMap::new();
                    if let Some(base_reg) = spread
                        && let Some(base) = regs[fbase + *base_reg as usize].shape()
                    {
                        let base_val = regs[fbase + *base_reg as usize];
                        for (i, field) in base.fields.iter().enumerate() {
                            let value = base_val.slots().unwrap()[i];
                            retain(value);
                            if let Some(old) = bag.insert(field.clone(), value) {
                                release(old);
                            }
                        }
                    }
                    for (key, reg) in keys.iter() {
                        let value = regs[fbase + *reg as usize];
                        retain(value);
                        if let Some(old) = bag.insert(module.name(*key).to_string(), value) {
                            release(old);
                        }
                    }
                    let fields: Vec<String> = bag.keys().cloned().collect();
                    let slots: Vec<Value> = bag.into_values().collect();
                    let shape = noeta_object::intern_shape(Shape::object(
                        ShapeKind::Opaque,
                        module.name(*type_name).to_string(),
                        fields,
                    ));
                    set_reg(regs, fbase, *dst, Value::object(shape, slots));
                    pc += 1;
                }
                Op::EnumFromStr {
                    dst,
                    arg,
                    enum_name,
                    cases,
                    some_shape,
                    none_shape,
                    panic,
                    span,
                } => {
                    let enum_name = module.name(*enum_name);
                    let value = regs[fbase + *arg as usize];
                    let Some(probe) = crate::values::wire_probe(value) else {
                        let kind = if *panic { "from" } else { "try_from" };
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`{enum_name}.{kind}` expects a string or a backing value, \
                             found {}",
                                value.type_name()
                            ),
                        ));
                    };
                    // Backing first, then case name — the shared rule the tree-walker also runs,
                    // over the case names and backings baked in at compile time.
                    let names: Vec<&str> = cases
                        .iter()
                        .map(|(name, _, _)| module.name(*name))
                        .collect();
                    let probe_cases: Vec<(&str, Option<&noeta_ast::AttrValue>)> = names
                        .iter()
                        .zip(cases.iter())
                        .map(|(name, (_, backing, _))| (*name, backing.as_ref()))
                        .collect();
                    let matched = noeta_ast::reflect::variant_for_wire(&probe_cases, &probe)
                        .map(|i| &cases[i]);
                    let result = match matched {
                        Some((_, _, shape_idx)) => {
                            // Build the payload-free case; its single reference transfers onward.
                            let shape = self.persist.shapes[*shape_idx as usize];
                            let case = Value::enum_value(shape, Vec::new());
                            if *panic {
                                case
                            } else {
                                let some = self.persist.shapes[*some_shape as usize];
                                Value::enum_value(some, vec![case])
                            }
                        }
                        None if *panic => {
                            let shown = value.display();
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                *span,
                                format!("panic: `{enum_name}` has no case `{shown}`"),
                            ));
                        }
                        None => {
                            let none = self.persist.shapes[*none_shape as usize];
                            Value::enum_value(none, Vec::new())
                        }
                    };
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::Panic { msg, span } => {
                    let message = regs[fbase + *msg as usize].display();
                    return Err(self.error(
                        DiagnosticCode::Panic,
                        *span,
                        format!("panic: {message}"),
                    ));
                }
                Op::TryUnwrap {
                    dst,
                    src,
                    on_error,
                    span,
                } => {
                    let v = regs[fbase + *src as usize];
                    match try_classify(v) {
                        Some(TryOutcome::Success(inner)) => {
                            retain(inner);
                            set_reg(regs, fbase, *dst, inner);
                            pc += 1;
                        }
                        // `Err(_)`/`none`: early-return the whole value from this frame, exactly
                        // as `Op::Return` does (the tree-walker's `Unwind::Return`).
                        Some(TryOutcome::Empty) => {
                            // An `Err` propagating out of the outermost run's BOTTOM frame is
                            // top-level code: there is no caller to hand it to, and no declared
                            // return type the checker could have rejected the `?` against (E0012
                            // covers every declared return). Unwinding here used to end the
                            // program *quietly at exit 0* — a `client.get(url)?` transport
                            // failure was completely invisible and CI went green on a broken
                            // program. Abort with the error's own message instead (E0069); the
                            // teardown in `Vm::run` reclaims this frame, exactly as `Op::Panic`'s
                            // does. The tree-walker's `eval_try_ir` mirrors this on an empty
                            // `call_sites`, so the differential holds. `none` is untouched: an
                            // absence reaching the top is not a failure.
                            if self.run_depth == 1
                                && frames.len() == 1
                                && let Some(payload) = crate::values::result_err_payload(v)
                            {
                                let message = self.unhandled_error_message(payload, *span)?;
                                self.out
                                    .diagnostics
                                    .push(Diagnostic::unhandled_error(*span, &message));
                                return Err(Abort);
                            }
                            retain(v);
                            // Drop the frame locals this `?` abandons before unwinding —
                            // destructor-relevant ones fire `destruct`, in the drop pass's order. Each
                            // is cleared to `unit`, so the teardown release below never double-frees.
                            for (reg, relevant) in on_error.iter() {
                                let dv = std::mem::replace(
                                    &mut regs[fbase + *reg as usize],
                                    Value::unit(),
                                );
                                if *relevant {
                                    self.release_value(dv);
                                } else {
                                    release(dv);
                                }
                            }
                            let finished = frames.pop().unwrap();
                            let n = module.protos[finished.proto as usize].num_registers as usize;
                            for i in 0..n {
                                release(regs[finished.base + i]);
                            }
                            for u in &finished.upvalues {
                                release(*u);
                            }
                            regs.truncate(finished.base);
                            // Apply the frame's return transform on every exit path, for the same
                            // reason `Op::Return` does (a short-circuiting `?` is an early return);
                            // release the original if the transform replaced it.
                            let (out, replaced) = finished.ret_transform.apply(v);
                            if replaced {
                                release(v);
                            }
                            match frames.last() {
                                Some(caller) => {
                                    let idx = caller.base + finished.ret_dst as usize;
                                    let old = regs[idx];
                                    regs[idx] = out;
                                    release(old);
                                }
                                None => return Ok(ColdStep::Done(out)),
                            }
                            // `?` short-circuits like an early return — re-derive the caller's window.
                            return Ok(ColdStep::Reload);
                        }
                        None => {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`?` expects a `Result` or `Option`, found {}",
                                    v.type_name()
                                ),
                            ));
                        }
                    }
                }
                Op::MakeGen { dst, src } => {
                    // Wrap the step closure into a generator iterator (Track G.1b). `iter_gen` retains
                    // its own reference to the closure; the source register's reference is released by
                    // the register's normal end-of-life (exactly as `Op::Narrow` retains its payload).
                    let step = regs[fbase + *src as usize];
                    let result = Value::iter_gen(step);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::MakeFuture { dst, src } => {
                    // Wrap the lazy thunk closure into a future (Track A.1). `make_future` retains its
                    // own reference to the closure; the source register's reference is released by the
                    // register's normal end-of-life (like `Op::MakeGen`).
                    let thunk = regs[fbase + *src as usize];
                    let result = Value::make_future(thunk);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::RunFuture { dst, src, span } => {
                    // Drive an awaited future to completion (Track A.2/A.3 top-level). See
                    // `drive_future`: poll; on pending advance the clock and re-poll; it borrows the
                    // future and returns an owned result. `.await` **consumes** the future (a spent
                    // future cannot be awaited again — a second await already deadlocks), so take it
                    // out of the source register and release it destructor-aware here, at its last
                    // reference: a destructor-bearing local captured in the async fn's state (held in
                    // the future's step-closure cells) runs now rather than being lost.
                    let future = std::mem::replace(&mut regs[fbase + *src as usize], Value::unit());
                    // Release before propagating an abort: the register was already emptied, so
                    // the frame teardown can no longer see the future — skipping this on the
                    // error path (e.g. a detected async deadlock) orphans it (the refcount
                    // anomaly the strengthened leak oracle catches). `drive_future` borrows.
                    // The consumed future rides `transient_roots` across the drive: with its
                    // register emptied, only this Rust local owns it, and a mid-drive safepoint
                    // collection must still see it as a root.
                    self.transient_roots.push(future);
                    let value = self.drive_future(future, *span, Some((frames, regs)));
                    self.transient_roots.pop();
                    self.release_value(future);
                    let value = value?;
                    set_reg(regs, fbase, *dst, value);
                    pc += 1;
                }
                Op::PollFuture { dst, src, span } => {
                    // Poll a future once (Track A.3 state machine): `some(v)` if ready, `none` if
                    // pending. The source register keeps owning the future.
                    let future = regs[fbase + *src as usize];
                    let result = match self.poll_once(future, *span)? {
                        Poll::Ready(value) => make_some(value),
                        // A cancelled handle awaited inside an `async fn` body would otherwise
                        // suspend forever on a `none`; fail loudly (Track A.8, E0056) instead —
                        // the same contract top-level `.await` (`drive_future`) enforces.
                        // A real isolate that honored its cancellation request (isolate-cancel)
                        // reads exactly like the cancelled handle below: the awaited work will
                        // never produce a value, so `.await` is the wrong tool for it.
                        Poll::Cancelled => {
                            return Err(self.error(
                                DiagnosticCode::AwaitCancelled,
                                *span,
                                "cannot await a cancelled task; use `.join()` to observe the \
                             cancelled outcome"
                                    .to_string(),
                            ));
                        }
                        Poll::Pending if self.handle_cancelled(future) => {
                            return Err(self.error(
                                DiagnosticCode::AwaitCancelled,
                                *span,
                                "cannot await a cancelled task; use `.join()` to observe the \
                             cancelled outcome"
                                    .to_string(),
                            ));
                        }
                        Poll::Pending => make_none(),
                    };
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::LoadPending { dst } => {
                    // The async pending sentinel (Track A.3) — what a step returns when it suspends.
                    set_reg(regs, fbase, *dst, Value::pending());
                    pc += 1;
                }
                Op::ScopeBegin => {
                    // Open a structured-concurrency scope (Track A.3b): a fresh, empty task list
                    // (A.7 tombstone model — `open_scope` pushes both `scopes` and `scope_closed`).
                    self.open_scope();
                    pc += 1;
                }
                Op::ScopeBeginValue { dst, .. } => {
                    // Open a scope and yield its index (Track A.7): the value form of `ScopeBegin`,
                    // used by the async desugar's split `concurrent { }` to thread the index to its
                    // join poll-state. Mirrors `noeta-eval`'s `Rvalue::ScopeBegin`.
                    let idx = self.open_scope() as i64;
                    set_reg(regs, fbase, *dst, Value::int(idx));
                    pc += 1;
                }
                Op::ScopeReady { dst, src, span } => {
                    // Whether every task in the scope at index `src` has completed or been cancelled
                    // (Track A.7) — the boolean the split `concurrent { }`'s join poll-state tests
                    // each poll. A stale/out-of-range index reads ready (defensive; unreachable for a
                    // clean program). Mirrors `noeta-eval`'s `Rvalue::ScopeReady`.
                    let Some(idx) = regs[fbase + *src as usize].as_int() else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "internal: scope_ready expects a scope index".to_string(),
                        ));
                    };
                    let ready = self
                        .sched
                        .scopes
                        .get(idx as usize)
                        .is_none_or(|s| s.iter().all(|t| t.result.is_some() || t.cancelled));
                    set_reg(regs, fbase, *dst, Value::bool(ready));
                    pc += 1;
                }
                Op::Spawn { dst, src, .. } => {
                    // Register the future as a task in the current scope (retaining the scope's own
                    // reference), yielding a handle that references it by `(scope, task)`. A `spawn`
                    // outside any scope is E0041 at check, so `self.scopes` is non-empty here.
                    let future = regs[fbase + *src as usize];
                    let handle = if self.sched.scopes.is_empty() {
                        retain(future);
                        future
                    } else {
                        retain(future);
                        // Senders captured in the spawned future are producer holds (isolates
                        // I.4c auto-close); count them onto their channels for the task's life.
                        let holds = Self::collect_producer_channels(future);
                        for &cid in &holds {
                            self.add_producer_hold(cid);
                        }
                        let scope_idx = self.innermost_open();
                        let task_idx = self.sched.scopes[scope_idx].len();
                        // The child inherits a snapshot of the spawner's task-local context:
                        // a task spawned inside `with_span` parents its spans there.
                        let context = self.sched.ctx_current.clone();
                        let strand = self.sched.current_strand;
                        self.sched.scopes[scope_idx].push(Task {
                            future,
                            result: None,
                            cancelled: false,
                            polling: false,
                            context,
                            holds,
                            strand,
                            isolate_strand: None,
                        });
                        Value::make_handle(
                            ScopeId::from_index(scope_idx),
                            TaskId::from_index(task_idx),
                        )
                    };
                    set_reg(regs, fbase, *dst, handle);
                    pc += 1;
                }
                Op::SpawnIsolate {
                    dst,
                    callee,
                    args,
                    span,
                } => {
                    // `isolate f(args)` (I.4b). Only the CLI's real (VM) path emits this op; the
                    // differential/salsa sandbox lowers `isolate` to `Call`+`Spawn`, so it is never
                    // reached in-oracle. Runs on a real OS thread when the VM is parallel and no
                    // argument ships a channel; otherwise falls back to a cooperative task (so a
                    // non-parallel VM — `@test`/`bench` — and channel-shipping isolates never regress).
                    let callee_val = regs[fbase + *callee as usize];
                    let arg_vals = ArgBuf::collect(args, regs, fbase);
                    let handle = self.spawn_isolate(callee_val, arg_vals.as_slice(), *span)?;
                    set_reg(regs, fbase, *dst, handle);
                    pc += 1;
                }
                Op::ScopeEnd { span } => {
                    // Join the scope (drive every task to completion), then close the innermost scope
                    // and release the tasks' owned futures and results (close_scope: producer holds
                    // I.4c + destructor-aware future release). The synchronous (non-flattened) path
                    // is strictly LIFO, so the innermost scope is this one.
                    self.join_scope(*span, Some((frames, regs)))?;
                    let si = self.innermost_open();
                    self.close_scope(si);
                    pc += 1;
                }
                Op::ScopeEndAt { src, span } => {
                    // Close the (already-drained) scope at index `src` (Track A.7): release its tasks
                    // (destructor-aware) and tombstone the slot. Closes by index — not innermost — so
                    // a sibling task's still-open scope above it survives. The join happened at the
                    // `ScopeReady` poll-state, so there is nothing to drive. Mirrors `noeta-eval`'s
                    // `Rvalue::ScopeEndAt`.
                    let Some(idx) = regs[fbase + *src as usize].as_int() else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "internal: scope_end expects a scope index".to_string(),
                        ));
                    };
                    if (idx as usize) < self.sched.scopes.len() {
                        self.close_scope(idx as usize);
                    }
                    pc += 1;
                }
                Op::MakeChannel {
                    dst,
                    capacity,
                    span,
                } => {
                    // Create a bounded channel and yield its `(Sender, Receiver)` endpoint tuple
                    // (isolates I.1). The message type is checker-only; only the capacity reaches here.
                    let cap = regs[fbase + *capacity as usize];
                    let Some(cap) = cap.as_int() else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`channel` expects an int capacity, found {}",
                                cap.type_name()
                            ),
                        ));
                    };
                    if cap < 0 {
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            *span,
                            format!("`channel` capacity must be non-negative, found {cap}"),
                        ));
                    }
                    let id = ChannelId::from_index(self.persist.channels.len());
                    // In a parallel VM (real isolates, I.4c) a channel is a *shared* cross-thread queue
                    // from birth, so shipping an endpoint into a worker shares one queue; the sandbox
                    // (and any non-parallel VM) uses the cooperative in-VM `Local` FIFO, unchanged.
                    let channel = if self.isolates.parallel_isolates {
                        Channel::Shared(isolate::ChannelCore::new(cap as usize))
                    } else {
                        Channel::Local {
                            buffer: std::collections::VecDeque::new(),
                            capacity: cap as usize,
                            closed: false,
                            producers: 0,
                        }
                    };
                    self.persist.channels.push(channel);
                    // The two endpoints are fresh (refcount 1); `Value::tuple` takes ownership of
                    // exactly those references, so no extra retain is needed.
                    let tuple =
                        Value::tuple(vec![Value::make_sender(id), Value::make_receiver(id)]);
                    set_reg(regs, fbase, *dst, tuple);
                    pc += 1;
                }
                Op::AttributesOf { dst, src } => {
                    // The manifest is name-keyed: the register holds the attribute type's name,
                    // folded from a written turbofish or read off a per-instantiation channel by
                    // the preceding `TypeArgName`/`TypeSlotName` — the same string either way.
                    let name = regs[fbase + *src as usize].as_string().unwrap_or_default();
                    let result = self.materialize_attributes(&name);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::RolesOf { dst, src } => {
                    // The optional scope arrives the same way; `None` is the unscoped query.
                    let filter =
                        src.map(|src| regs[fbase + src as usize].as_string().unwrap_or_default());
                    let result = self.materialize_roles(filter.as_deref());
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::ParamsOf { dst, src } => {
                    // The runtime target string names a fn or method; materialize its params.
                    let target = regs[fbase + *src as usize].as_string().unwrap_or_default();
                    let result = self.materialize_params(&target);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::ReturnsOf { dst, src } => {
                    // The runtime target string names a fn or method; materialize its return.
                    let target = regs[fbase + *src as usize].as_string().unwrap_or_default();
                    let result = self.materialize_returns(&target);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::FieldSpecsOf { dst, src } => {
                    // The runtime name string names a declared type; materialize its field schema.
                    let name = regs[fbase + *src as usize].as_string().unwrap_or_default();
                    let result = self.materialize_field_specs(&name);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::VariantsOf { dst, src } => {
                    // The runtime name string names a declared type; materialize its variant
                    // schema (empty unless it is an enum).
                    let name = regs[fbase + *src as usize].as_string().unwrap_or_default();
                    let result = self.materialize_variant_specs(&name);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::Construct {
                    dst,
                    name,
                    fields,
                    ok_shape,
                    err_shape,
                    span,
                } => {
                    let name_val = regs[fbase + *name as usize];
                    let fields_val = regs[fbase + *fields as usize];
                    let result =
                        self.construct_dynamic(name_val, fields_val, *ok_shape, *err_shape, *span)?;
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::TypeOf { dst, src } => {
                    let repr = vm_type_repr(&regs[fbase + *src as usize]);
                    let result = build_type_value(&repr);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::TypeArgName {
                    dst,
                    src,
                    index,
                    names,
                    span,
                } => {
                    let repr = vm_type_repr(&regs[fbase + *src as usize]);
                    let Some(name) = repr.type_arg_name(*index as usize) else {
                        return Err(self.error(
                            DiagnosticCode::InvalidTypeArguments,
                            *span,
                            noeta_ast::reflect::missing_type_arg_message(&names.0, &names.1),
                        ));
                    };
                    let result = Value::string(&name);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::SelfRenderSlot { dst, src, index } => {
                    // The receiver's own tag is the instantiation an instance method of a generic
                    // type cannot carry on a hidden slot — the same read `Op::TypeArgName` above
                    // performs, answering with the table index a hint resolves through instead of
                    // a name, and degrading to `NO_TYPE_ARG` where that one aborts.
                    //
                    // The table's reflection projection is interned through the repr pool here, so
                    // it is rebuilt as a borrow rather than cloned; the tree-walker holds the same
                    // reprs inline and hands the same sequence to the same helper.
                    let value = regs[fbase + *src as usize];
                    let reprs = || {
                        module
                            .type_arg_reprs
                            .iter()
                            .map(|r| r.and_then(|k| module.type_reprs.get(k as usize)))
                    };
                    // `vm_type_repr` clones the tag it finds; borrowing it when it is there keeps a
                    // door inside a loop from allocating one per iteration, and the fallback is
                    // that very function, so the two arms cannot answer differently.
                    let slot = match value.reflect() {
                        Some(tag) => tag.render_slot_arg(*index as usize, reprs()),
                        None => vm_type_repr(&value).render_slot_arg(*index as usize, reprs()),
                    };
                    set_reg(regs, fbase, *dst, Value::int(slot));
                    pc += 1;
                }
                // A render slot this frame composes out of its own leaf slots, because the
                // instantiation is one the body BUILT and no slot carries that type whole. The
                // lookup is the shared one, so the tree-walker cannot compose a different entry
                // from the same leaves; an unlisted combination is `NO_TYPE_ARG` and the value
                // renders as its erased word.
                Op::ComposeTypeArg { dst, slots, cases } => {
                    let leaves: Vec<i64> = slots
                        .iter()
                        .map(|r| {
                            regs[fbase + *r as usize]
                                .as_int()
                                .unwrap_or(noeta_stdlib::NO_TYPE_ARG)
                        })
                        .collect();
                    let composed =
                        noeta_stdlib::compose_type_arg(cases, &module.type_arg_hints, &leaves);
                    set_reg(regs, fbase, *dst, Value::int(composed));
                    pc += 1;
                }
                Op::TypeSlotName { dst, src, span } => {
                    let idx = regs[fbase + *src as usize].as_int().unwrap_or(-1);
                    let Some(entry) = module.type_args.get(idx.max(0) as usize) else {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "corrupt hidden type-argument slot".to_string(),
                        ));
                    };
                    let result = Value::string(&entry.name);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::FieldsOf {
                    dst,
                    src,
                    private_fields,
                } => {
                    let result =
                        self.materialize_fields(regs[fbase + *src as usize], *private_fields);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::TraitsOf { dst, src } => {
                    let result = self.materialize_traits(regs[fbase + *src as usize]);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::Retag { reg, repr } => {
                    regs[fbase + *reg as usize]
                        .set_reflect(Some(Rc::clone(&self.persist.type_reprs[*repr as usize])));
                    pc += 1;
                }
                Op::RetagDynamic { reg, slot } => {
                    let idx = regs[fbase + *slot as usize].as_int().unwrap_or(-1);
                    if idx >= 0
                        && let Some(Some(repr)) = module.type_arg_reprs.get(idx as usize).copied()
                    {
                        regs[fbase + *reg as usize]
                            .set_reflect(Some(Rc::clone(&self.persist.type_reprs[repr as usize])));
                    }
                    pc += 1;
                }
                Op::TypeOfStatic { dst, repr } => {
                    let result = build_type_value(repr);
                    set_reg(regs, fbase, *dst, result);
                    pc += 1;
                }
                Op::TypeValue { dst, name } => {
                    // A bare type name used as a value (an `invoke` receiver) materializes as the
                    // reflection `Type` ADT — the one representation of "a type as a value", shared
                    // with `type_of` and stored type-refs. `Op::Invoke` resolves it back to the
                    // named type via `reflection_type_name`.
                    // No type arguments: the op carries a single name index because the surface
                    // form it lowers from is a bare identifier in receiver position
                    // (`invoke(Foo, …)`), which cannot spell a generic application.
                    let value =
                        build_type_value(&module.reflection.type_ref_repr(module.name(*name), &[]));
                    set_reg(regs, fbase, *dst, value);
                    pc += 1;
                }
                Op::Invoke {
                    dst,
                    recv,
                    name,
                    args,
                    ok_shape,
                    err_shape,
                    span,
                } => {
                    let recv_val = recv.map(|r| regs[fbase + r as usize]);
                    let name_val = regs[fbase + *name as usize];
                    let args_val = regs[fbase + *args as usize];
                    // A packed args list is materialized to a temporary boxed list for
                    // the duration of the dispatch, then released after the call frame is built (its
                    // elements retained into it). `arg_items` below borrows from this temporary.
                    let mut args_to_release: Option<Value> = None;
                    // A **named** `Map<string, dyn>` args operand (the same shape `Op::Construct`
                    // takes) is projected to its `(name, value)` pairs here and bound to
                    // parameters once the callee is known. The values share the map's references
                    // exactly as `construct_dynamic`'s do, so there is no temporary to release;
                    // a non-string key is dropped, as it is there.
                    let named: Option<Vec<(String, Value)>> = args_val.is_map().then(|| {
                        let keys = args_val.map_keys().expect("checked is_map");
                        let vals = args_val.map_values().expect("checked is_map");
                        keys.iter()
                            .zip(vals)
                            .filter_map(|(k, v)| match k {
                                noeta_stdlib::MapKey::Str(s) => Some((s.as_str().to_owned(), v)),
                                _ => None,
                            })
                            .collect()
                    });
                    // Resolve the dispatch by name: either a prototype to call (`Ok`) or a reason it
                    // failed (`Err(msg)` → `Result.Err`). Every resolution failure — non-string name,
                    // args that are neither a list nor a map, a non-invokable receiver, an unknown
                    // name, an arity mismatch, an unknown or missing parameter in the named form —
                    // is a runtime `Err`, never an abort (only a panic *inside* the called body
                    // aborts).
                    let outcome: Result<(InvokeTarget, Vec<Value>, Option<u64>), String> = 'resolve: {
                        let Some(method) = name_val.as_string() else {
                            break 'resolve Err(format!(
                                "invoke name must be a string, found {}",
                                name_val.type_name()
                            ));
                        };
                        if named.is_none() && !args_val.is_list() {
                            break 'resolve Err(format!(
                                "invoke args must be a list or a map, found {}",
                                args_val.type_name()
                            ));
                        }
                        // A packed positional list is materialized to a temporary boxed list for
                        // the duration of the dispatch; the named form reads the map directly.
                        let arg_items: Vec<Value> = if named.is_some() {
                            Vec::new()
                        } else {
                            let args_list = args_val.realize_list();
                            args_to_release = Some(args_list);
                            args_list.list_items().expect("checked is_list")
                        };
                        // No receiver: the free-function form. The name resolves against the
                        // module's global slot table and nothing else — the same binding
                        // `Op::CallGlobal` reads for a statically-known top-level `fn`, which is
                        // what makes the two-argument form pair with `params_of("name")`. A
                        // global that is absent, unbound, or holds a non-closure is one and the
                        // same miss (see `free_fn_miss_message`).
                        let Some(recv_val) = recv_val else {
                            let callee = self
                                .global_slots
                                .get(&*method)
                                .map(|slot| self.persist.globals[*slot as usize])
                                .filter(|v| !v.is_unbound() && v.as_closure().is_some());
                            let Some(callee) = callee else {
                                break 'resolve Err(free_fn_miss_message(&method));
                            };
                            // A free fn reserves no `self` register, so its declared arity is
                            // the parameter count itself — unlike the method path below — less
                            // a forwarding generic's hidden type-argument slots, which are not
                            // part of the surface signature the reflection artifact describes.
                            // Counting them would report an arity the source never wrote;
                            // `invoke` supplies no instantiation, so the CALL is what refuses
                            // (see `Vm::no_instantiation`), and it must be reached to do so.
                            let chunk =
                                &module.protos[callee.as_closure().expect("filtered") as usize];
                            let total = chunk.num_params as usize - chunk.hidden as usize;
                            break 'resolve match bind_invoke_args(
                                &module.reflection,
                                &method,
                                "function",
                                total,
                                chunk.defaults.len(),
                                arg_items,
                                named.as_deref(),
                            ) {
                                Ok((args, supplied)) => {
                                    Ok((InvokeTarget::Free(callee), args, supplied))
                                }
                                Err(message) => Err(message),
                            };
                        };
                        // A type handle dispatches an associated function (no receiver); an object
                        // dispatches an instance method (receiver in register 0). A reflection `Type`
                        // value (a stored type-ref) names the type for an associated call too.
                        let (type_name, is_assoc) = if recv_val.is_object() {
                            (recv_val.shape().unwrap().name.clone(), false)
                        } else if let Some(tn) = reflection_type_name(recv_val) {
                            (tn, true)
                        } else {
                            break 'resolve Err(format!(
                                "cannot invoke on a value of type `{}`",
                                recv_val.type_name()
                            ));
                        };
                        let kind = if is_assoc {
                            "static function"
                        } else {
                            "method"
                        };
                        let Some(proto) = self.method_proto(&type_name, &method) else {
                            // A method a `@derive`d built-in trait makes callable. Arity is bound
                            // here like every other target's, so a wrong count is the `Result.Err`
                            // `invoke` promises rather than an abort.
                            if !is_assoc
                                && let Some(row) =
                                    noeta_ast::derive::derived_builtin_method(&method)
                                && module
                                    .reflection
                                    .type_implements(&type_name, row.trait_name)
                            {
                                break 'resolve match bind_invoke_args(
                                    &module.reflection,
                                    &format!("{type_name}.{method}"),
                                    kind,
                                    row.arity,
                                    0,
                                    arg_items,
                                    named.as_deref(),
                                ) {
                                    Ok((args, supplied)) => {
                                        Ok((InvokeTarget::Derived(row), args, supplied))
                                    }
                                    Err(message) => Err(message),
                                };
                            }
                            // A bare `from` names no single conversion; say which ones exist.
                            break 'resolve Err(
                                self.missing_method_message(&type_name, &method, is_assoc)
                            );
                        };
                        // The prototype reserves register 0 for `self` (unit for an associated
                        // call), so its declared arity is one more than the supplied args; trailing
                        // defaults widen the accepted range, exactly as `Op::CallMethod`.
                        let callee_chunk = &module.protos[proto as usize];
                        // `invoke` reckons arity over the VALUE parameters — a forwarding
                        // generic's hidden slots are not part of the surface signature the
                        // reflection artifact describes. It supplies none, so the push itself
                        // aborts; reporting an arity the source never wrote first would only
                        // mislead.
                        let total =
                            callee_chunk.num_params as usize - 1 - callee_chunk.hidden as usize;
                        let (args, supplied) = match bind_invoke_args(
                            &module.reflection,
                            &format!("{type_name}.{method}"),
                            kind,
                            total,
                            callee_chunk.defaults.len(),
                            arg_items,
                            named.as_deref(),
                        ) {
                            Ok(bound) => bound,
                            Err(message) => break 'resolve Err(message),
                        };
                        Ok((InvokeTarget::Proto { proto, is_assoc }, args, supplied))
                    };
                    match outcome {
                        Err(message) => {
                            let shape = self.persist.shapes[*err_shape as usize];
                            let err = Value::enum_value(shape, vec![Value::string(&message)]);
                            set_reg(regs, fbase, *dst, err);
                            pc += 1;
                        }
                        // A free function has no receiver register and no `self` slot, so it
                        // cannot ride `push_callee_frame` (which reserves register 0). It runs
                        // through the ordinary first-class-callee path instead — the same
                        // re-entry `map`/`filter` use — which carries the closure's upvalues and
                        // fills its defaults. Arity was pre-checked above, so `call_value`'s own
                        // (aborting) arity guard is unreachable and a soft `Err` is preserved.
                        Ok((InvokeTarget::Free(callee), arg_items, supplied)) => {
                            // `call_value_masked` consumes owned arguments; the list (or, in the
                            // named form, the map) still owns these.
                            let owned: Vec<Value> = arg_items
                                .iter()
                                .map(|&a| {
                                    retain(a);
                                    a
                                })
                                .collect();
                            if let Some(list) = args_to_release.take() {
                                list.release();
                            }
                            let result = self.call_value_masked(callee, owned, *span, supplied)?;
                            let ok = self.persist.shapes[*ok_shape as usize];
                            set_reg(regs, fbase, *dst, Value::enum_value(ok, vec![result]));
                            pc += 1;
                        }
                        Ok((InvokeTarget::Derived(row), arg_items, _)) => {
                            // No frame to push: the structural routine runs here and its result is
                            // wrapped in `Result.Ok` exactly as a returning body's would be. The
                            // receiver and arguments stay borrowed — the routine reads them and
                            // hands back a fresh value.
                            let recv = recv_val.expect("an instance dispatch resolved a receiver");
                            let result =
                                self.derived_builtin_call(recv, row.method, &arg_items, *span);
                            if let Some(list) = args_to_release.take() {
                                list.release();
                            }
                            let value = result.expect("the row resolved on this receiver")?;
                            let ok = self.persist.shapes[*ok_shape as usize];
                            set_reg(regs, fbase, *dst, Value::enum_value(ok, vec![value]));
                            pc += 1;
                        }
                        Ok((InvokeTarget::Proto { proto, is_assoc }, arg_items, supplied)) => {
                            // An associated call leaves register 0 as unit (no receiver); an instance
                            // call places the retained receiver there. The result is wrapped in
                            // `Result.Ok` as it lands in the caller, so the invocation yields a
                            // `Result` whichever way the body returns.
                            // An instance dispatch always resolved through a receiver, so the
                            // flatten never discards one: `is_assoc` is false only on the
                            // `recv_val.is_object()` arm.
                            let recv = (!is_assoc).then_some(recv_val).flatten();
                            let ok = self.persist.shapes[*ok_shape as usize];
                            self.push_callee_frame(
                                frames,
                                regs,
                                top,
                                proto,
                                recv,
                                &arg_items,
                                // `invoke` is name-keyed with no static callee type, so it
                                // carries no instantiation: a forwarding generic reached this
                                // way aborts rather than bind a value argument into a type slot.
                                &[],
                                *dst,
                                RetTransform::WrapOk(ok),
                                pc + 1,
                                // The prototype reserves register 0 for `self`, so every
                                // declared parameter's bit moves up by one — the same shift the
                                // compiler applies to a statically-named method call.
                                supplied.map(|m| (m << 1) | 1),
                                *span,
                            )?;
                            // Release the temporary boxed args list before transferring (its
                            // elements were already retained into the call frame above); `take`
                            // leaves the after-match release for the non-transferring `Err` path.
                            if let Some(list) = args_to_release.take() {
                                list.release();
                            }
                            return Ok(ColdStep::Reload);
                        }
                    }
                    // Release the temporary boxed args list (if the args were materialized from a
                    // packed list); its elements were retained into the call frame above.
                    if let Some(list) = args_to_release {
                        list.release();
                    }
                }
                Op::MaskWidth {
                    dst,
                    src,
                    signed,
                    bits,
                } => {
                    // Reduce an erased fixed-width integer (an `int` value) into its declared width
                    // (Tier W). Total — the shared helper runs identically in the tree-walker. A
                    // non-int (only if the checker's IntN guarantee broke) passes through unchanged.
                    //
                    // Ownership: a masked result is a *fresh* value from `Value::int` — already
                    // owning its one reference if it heap-boxes (a `u64` past the immediate range),
                    // so it must NOT be retained again (the refcount-anomaly oracle catches the
                    // over-count as a leak). Only the pass-through borrows from the src register
                    // and needs the retain for its new owner.
                    let v = regs[fbase + *src as usize];
                    let masked = match v.as_int() {
                        Some(n) => Value::int(noeta_stdlib::mask_to_width(n, *signed, *bits)),
                        None => {
                            retain(v);
                            v
                        }
                    };
                    set_reg(regs, fbase, *dst, masked);
                    pc += 1;
                }
                Op::WideInt {
                    op,
                    dst,
                    a,
                    b,
                    signed,
                    bits,
                    span,
                } => {
                    // Sign-dependent fixed-width op: `/ % < <= > >=` on erased-int operands,
                    // read as `signed`/unsigned `bits`-wide. No trait dispatch (ints only).
                    let left = regs[fbase + *a as usize];
                    let right = regs[fbase + *b as usize];
                    match apply_binary_wide(*op, left, right, *signed, *bits) {
                        Ok(v) => {
                            set_reg(regs, fbase, *dst, v);
                            pc += 1;
                        }
                        Err(e) => return Err(self.error(e.code, *span, e.text)),
                    }
                }
                Op::WidthIntMethod {
                    dst,
                    recv,
                    method,
                    arg,
                    bits,
                    ..
                } => {
                    // An int method needing the receiver's static width: a bit intrinsic
                    // computes within `bits` rather than the erased i64; a range-checked conversion
                    // answers an option. The checker guarantees an integer receiver and (for
                    // `rotate_*`) an integer arg.
                    let recv_int = regs[fbase + *recv as usize].as_int().unwrap_or(0);
                    let amount = match arg {
                        Some(r) => regs[fbase + *r as usize].as_int().unwrap_or(0),
                        None => 0,
                    };
                    let value = match noeta_stdlib::int_method_outcome(
                        recv_int,
                        *method,
                        amount,
                        Some(*bits),
                    ) {
                        noeta_stdlib::IntOutcome::Word(word) => Value::int(word),
                        noeta_stdlib::IntOutcome::Checked(Some(word)) => {
                            make_some(Value::int(word))
                        }
                        noeta_stdlib::IntOutcome::Checked(None) => make_none(),
                    };
                    set_reg(regs, fbase, *dst, value);
                    pc += 1;
                }
                Op::Raise { idx } => {
                    self.out
                        .diagnostics
                        .push(chunk.diagnostics[*idx as usize].clone());
                    return Err(Abort);
                }
                // Unreachable by construction: `dispatch_inner` routes exactly the patterns
                // listed above here, and it still matches `Op` exhaustively, so a newly added
                // opcode fails to compile *there* — where the hot/cold decision belongs — rather
                // than falling through to this arm.
                _ => unreachable!("cold_op reached with an op the dispatch loop handles inline"),
            }
        }
        Ok(ColdStep::Next(pc))
    }

    /// The shared callee-frame-push choreography (audit-1 finding 6): reserve the callee's
    /// register window, retain the receiver and arguments into it, fill omitted trailing
    /// defaults from their thunks, save the caller's resume pc, and push the callee frame.
    /// Extracted from the near-verbatim copies in the object/enum `Op::CallMethod` arms,
    /// `Op::Invoke`, `Op::Binary`'s operator dispatch, and the single-argument trait
    /// dispatches (`Iterable::iter`, `Length::len`, `Index::get`, `Display::to_string`) —
    /// the retain/default-thunk choreography is exactly where refcount bugs live, so it
    /// exists once.
    ///
    /// Contract points, all preserved from the arms:
    /// - **Arity is the caller's job.** The arms report violations on different channels
    ///   (abort vs. `Op::Invoke`'s soft `Result.Err`), so this function assumes a fitting
    ///   `args` slice.
    /// - `recv: None` leaves register 0 unit (an associated `Invoke`); `Some` is retained in.
    /// - `args` are **borrowed** (register/list-owned) values, each retained into the window.
    /// - Frames pushed here carry no upvalues (methods/operators are defined at module
    ///   scope), so default thunks resolve globals only; register 0 counts as filled whether
    ///   or not a receiver was placed, exactly as every arm computed `filled = args + 1`.
    /// - The caller ends its arm with `continue 'reload` — this only stages state.
    ///
    /// `#[inline]` so each (monomorphic) arm folds it back into the dispatch loop — the same
    /// contract as the `call_builtin_method` extraction (A/B-benched at ±0).
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn push_callee_frame(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        top: usize,
        proto: u32,
        recv: Option<Value>,
        args: &[Value],
        ty_args: &[Value],
        ret_dst: u16,
        ret_transform: RetTransform,
        resume_pc: usize,
        supplied: Option<u64>,
        span: Span,
    ) -> Result<(), Abort> {
        let module = self.module;
        let callee_chunk = &module.protos[proto as usize];
        // A forwarding generic METHOD (Axis A) declares hidden type-argument slots; only a call
        // with a static receiver type can fill them. The name-keyed entry points that route
        // through here without one — `invoke`, a method handle, a `dyn` receiver — pass none, and
        // binding positionally anyway would lay a value argument into a type slot.
        let hidden = callee_chunk.hidden as usize;
        if ty_args.len() != hidden {
            let name = callee_chunk.name.clone();
            return Err(self.no_instantiation(name.as_deref(), hidden, ty_args.len(), span));
        }
        let hidden_base = callee_chunk.hidden_base as usize;
        let new_base = reserve_window(regs, callee_chunk.num_registers as usize);
        if let Some(r) = recv {
            retain(r);
            regs[new_base] = r;
        }
        // The hidden block sits immediately after the receiver.
        for (j, &t) in ty_args.iter().enumerate() {
            retain(t);
            regs[new_base + hidden_base + j] = t;
        }
        // Register 0 holds the receiver, so an argument lands one past the parameter it fills —
        // and one hidden block further on, at a forwarding callee.
        for (i, &a) in args.iter().enumerate() {
            retain(a);
            let p = noeta_bytecode::param_of_arg(i + 1, supplied);
            regs[new_base + noeta_bytecode::reg_of_param(p, hidden, hidden_base)] = a;
        }
        // Fill every parameter the call left out from its default thunk — the trailing ones under
        // the ordinary prefix rule, plus any the mask says was skipped. A default's register is
        // absolute, so shift it back out of the hidden block to index the mask (a hidden slot
        // never carries a default, so the subtraction is always in range).
        if !callee_chunk.defaults.is_empty() {
            let defaults = callee_chunk.defaults.clone();
            let n_args = args.len() + 1;
            for (reg, dproto) in &defaults {
                if !noeta_bytecode::is_param_filled(*reg as usize - hidden, n_args, supplied) {
                    let value = self.run_thunk(*dproto, &[])?;
                    regs[new_base + *reg as usize] = value;
                }
            }
        }
        frames[top].pc = resume_pc;
        frames.push(Frame {
            proto,
            base: new_base,
            pc: 0,
            ret_dst,
            ret_transform,
            upvalues: Vec::new(),
        });
        Ok(())
    }
}

/// A stack-allocated argument buffer for built-in dispatch (string/list/map/set/iter methods,
/// prelude builtins, native modules). Those paths borrow their arguments as a `&[Value]`, and
/// collecting the argument registers into a heap `Vec` paid an allocation + free on **every**
/// such call — measurable on map/string loops, where the call ceremony, not the collection
/// operation itself, dominates. Arities are tiny (the stdlib tops out at three), so up to
/// [`ArgBuf::INLINE`] arguments live on the dispatch stack frame; a wider call (none exists in
/// the stdlib today) falls back to the heap rather than imposing a hidden arity cap.
pub(crate) enum ArgBuf {
    Inline([Value; ArgBuf::INLINE], usize),
    Heap(Vec<Value>),
}

impl ArgBuf {
    pub(crate) const INLINE: usize = 8;

    /// Copy the argument registers out of the frame window. The registers keep ownership
    /// (arguments are borrowed by every consumer), exactly as the `Vec` collect did.
    #[inline]
    pub(crate) fn collect(args: &[Reg], regs: &[Value], base: usize) -> Self {
        if args.len() <= Self::INLINE {
            let mut buf = [Value::unit(); Self::INLINE];
            for (slot, r) in buf.iter_mut().zip(args) {
                *slot = regs[base + *r as usize];
            }
            ArgBuf::Inline(buf, args.len())
        } else {
            ArgBuf::Heap(args.iter().map(|r| regs[base + *r as usize]).collect())
        }
    }

    #[inline]
    pub(crate) fn as_slice(&self) -> &[Value] {
        match self {
            ArgBuf::Inline(buf, n) => &buf[..*n],
            ArgBuf::Heap(v) => v,
        }
    }
}

/// Overwrite a register, releasing the value it held.
/// Map an arithmetic operator to its element-wise list op: `+`/`-`/`*` fold two lists
/// element-wise; every other operator has no list form (`None` → the scalar `apply_binary`).
pub(crate) fn elem_bin_op(op: noeta_ast::BinaryOp) -> Option<noeta_stdlib::ElemBinOp> {
    Some(match op {
        noeta_ast::BinaryOp::Add => noeta_stdlib::ElemBinOp::Add,
        noeta_ast::BinaryOp::Sub => noeta_stdlib::ElemBinOp::Sub,
        noeta_ast::BinaryOp::Mul => noeta_stdlib::ElemBinOp::Mul,
        _ => return None,
    })
}

/// The narrowing target a [`Op::Narrow`]/[`Op::IsType`] with a **dynamic** head-name register
/// resolves to, or `None` when the op carries no such register (the ordinary statically-written
/// target, which stays authoritative).
///
/// The register holds the instantiation's qualified name as a string — put there by the same
/// `TypeArgName`/`TypeSlotName` that serves `type_name::<T>()` — and it resolves through
/// [`NarrowTarget::from_runtime_name`], the funnel the compiler reduces a written type name through.
/// That shared funnel is what makes `v.as<T>()` at `T = int` answer what `v.as<int>()` does, and it
/// mirrors the tree-walker, which re-enters its own `runtime_matches` on the name for the same reason.
///
/// A non-string register cannot arise from a checked program (both producers write a string), and a
/// narrow has no failure channel, so it degrades to the baked target rather than aborting.
pub(crate) fn runtime_narrow_target(
    regs: &[Value],
    fbase: usize,
    dynamic: Option<u16>,
) -> Option<NarrowTarget> {
    let reg = dynamic?;
    regs[fbase + reg as usize].with_str(NarrowTarget::from_runtime_name)
}

/// Overwrite a register, releasing the value it held.
///
/// `#[inline(always)]`, not a plain hint: this is four instructions behind ~125 call sites in
/// `dispatch_inner`, and it inlined into all of them until `dispatch_inner` grew past LLVM's
/// inlining budget for a caller that large. Measured when it stopped: 3.0% of the interpreter
/// `loop` profile appeared as a *call* to a leaf this small, and the tier-0 benchmarks moved
/// 7–11% in instructions retired. The budget only ever gets tighter as the dispatch loop takes
/// on more ops, so the attribute states the requirement rather than re-earning it each release —
/// the same reason `MapKey::cmp` carries one.
#[inline(always)]
pub(crate) fn set_reg(regs: &mut [Value], base: usize, dst: u16, value: Value) {
    let idx = base + dst as usize;
    let old = regs[idx];
    regs[idx] = value;
    release(old);
}

/// Reserve a fresh `n`-slot register window at the top of the dispatch register stack for a callee
/// frame and return its base (P-VMT-FRAME). Slots are `unit`-initialized; the caller writes the
/// receiver/arguments into `regs[base..]` and pushes a `Frame { base, .. }`. Growing the stack may
/// reallocate the backing buffer, so no borrow into `regs` may be held across this call — access is
/// always by `(base, index)`.
pub(crate) fn reserve_window(regs: &mut Vec<Value>, n: usize) -> usize {
    let base = regs.len();
    if base + n > regs.capacity() {
        // Growing: initialize the whole new capacity (fill to capacity, then shrink back), so
        // the JIT's fast call convention may later extend `len` over it with `set_len` — every
        // element within capacity has then been written at least once (`set_len`'s contract).
        // Runs only on the rare growth, not per call.
        regs.reserve(base + n - regs.len());
        let cap = regs.capacity();
        regs.resize(cap, Value::unit());
        regs.truncate(base);
    }
    regs.resize(base + n, Value::unit());
    base
}

// --- Shared leaf-op happy paths (audit-1 finding 2a) --------------------------------------------
//
// The tier-1 JIT's `jit_run_leaf_op` (tier1.rs) runs the NON-dispatching, NON-erroring path of a
// handful of collection ops natively-called from compiled code; those paths were verbatim copies
// of the interpreter arms above. The shared computation now lives here as `#[inline]` pure-compute
// free functions BOTH call (`#[inline(always)]`: the dispatch loop is a huge inlining site a
// plain hint loses — A/B measured the plain-#[inline] form costing ~2% on the index loop): `None`/
// precondition failure means "not the happy path" — the
// interpreter raises its exact diagnostic, the leaf-op helper bails to the interpreter (which
// re-runs the op and raises the same diagnostic). Each helper performs **no register write** and
// no failure-side effect, preserving the leaf-op contract that every early return happens before
// any register write. The residual duplication in `jit_run_leaf_op` — the map/string `Op::Index`
// cases and `Op::LoadField` (interpreter-side inline cache, measured not-profitable for tier 1) —
// stays at its call sites with the divergence documented there.
//
// Not every shared leaf path is a free function here: where the op's happy path already needs the
// VM (`Op::CallMethod`'s map routes need `release_value` for a displaced value, and the diagnostic
// builders for their error cases), the ONE implementation is the existing `impl Vm` method — the
// interpreter arm and the leaf-op helper both call `Vm::map_update_in_place` /
// `Vm::call_builtin_method` unchanged. Same rule, wider seam: one behavior, one implementation,
// two callers.

/// `Op::Stringify`'s happy path: nothing dispatches, so the value passes through unchanged —
/// retained for its destination, because the consuming `Echo`/`BuildString` renders it with
/// `display`. The `Display` dispatch (a user object/enum with a `to_string` method) is the
/// caller's precondition: the interpreter tries it first, the JIT bails on **any** object/enum
/// receiver (a conservative superset — an object without `to_string` merely re-runs in tier 0).
#[inline(always)]
pub(crate) fn stringify_passthrough(v: Value) -> Value {
    retain(v);
    v
}

/// `Op::BuildString`'s whole computation — one pass, one output allocation (P-VMT-STR), no
/// failure path. The buffer is sized from the literal segments (known up front); holes grow it as
/// they render *into* it (no per-hole `display()` clone). Each hole register holds an
/// already-`Stringify`-ed value, so `display_into` never needs to dispatch — the whole build stays
/// within this one op. Holes are read by value (`Value` is `Copy`); their registers keep ownership.
#[inline(always)]
pub(crate) fn build_string(
    parts: &[StrPart],
    consts: &[Const],
    regs: &[Value],
    base: usize,
) -> Value {
    let cap: usize = parts
        .iter()
        .map(|p| match p {
            StrPart::Literal(k) => match &consts[*k as usize] {
                Const::Str(s) => s.len(),
                _ => 0,
            },
            StrPart::Hole(_) => 0,
        })
        .sum();
    let mut out = noeta_value::CompactString::with_capacity(cap);
    for part in parts.iter() {
        match part {
            StrPart::Literal(k) => {
                if let Const::Str(s) = &consts[*k as usize] {
                    out.push_str(s);
                }
            }
            StrPart::Hole(r) => regs[base + *r as usize].display_into(&mut out),
        }
    }
    // Move the finished buffer into the heap string — no second copy.
    Value::from_string(out)
}

/// `Op::ConcatInPlace`'s whole computation — no failure path. `l` is the **consumed** left operand:
/// the caller has already cleared its register *without* releasing (a direct overwrite, not
/// `set_reg`), so the refcount tests below still count the accumulator's reference and the single
/// owner transfers into the result — which also makes a `dst == lhs` store safe.
///
/// Sole-owner accumulators mutate in place (a flat packed list extends its word buffer, a boxed
/// list its backing buffer, a string its byte buffer), which is what makes `s = s ~ x` / `xs = xs ~
/// ys` in a loop O(n) instead of O(n²). Every aliased case copies, preserving immutable semantics.
#[inline(always)]
pub(crate) fn concat_in_place(l: Value, r: Value) -> Value {
    if l.is_list() && r.is_list() {
        if l.is_packed_list()
            && r.is_packed_list()
            && l.is_uniquely_owned()
            && l.packed_extend_in_place(r)
        {
            // Sole owner, both flat, same layout: append `rhs`'s words to `lhs`'s buffer in place
            //. The single reference moves into the result.
            l
        } else if !l.is_packed_list() && !r.is_packed_list() && l.is_uniquely_owned() {
            // Sole owner, both boxed: extend the backing buffer in place (O(1) amortized). The
            // single reference moves from `lhs` into the result.
            l.list_extend(r);
            l
        } else if let Some(flat) = l.packed_concat(r) {
            // Aliased but both flat (same layout): copy the word buffers, then drop the consumed
            // accumulator reference — stays flat without mutating the alias.
            release(l);
            flat
        } else {
            // A mixed packed/boxed pairing (or differing layouts): copy, preserving immutable
            // semantics. Demote each operand to an owned boxed list, retain each element into the
            // new list, release the demotions, then drop the accumulator's consumed reference.
            let lb = l.realize_list();
            let rb = r.realize_list();
            let mut items = lb.list_items().unwrap();
            items.extend(rb.list_items().unwrap());
            for &item in &items {
                item.inc_ref();
            }
            lb.release();
            rb.release();
            release(l);
            Value::list(items)
        }
    } else if l.is_string() && l.is_uniquely_owned() {
        // Sole owner of a string accumulator: append `rhs`'s display form to its buffer in place
        // (amortized O(1)), mirroring the list path — the single reference moves into the result.
        l.str_push_in_place(&r.display());
        l
    } else {
        // Aliased accumulator or non-string lhs: display concatenation into a fresh string
        // (preserves immutable semantics), identical to `Op::Binary`'s `~`.
        let s = Value::string(&format!("{}{}", l.display(), r.display()));
        release(l);
        s
    }
}

/// `Op::MakeRange`'s happy path: int bounds → the materialized element list (refcount 1).
/// `None` = non-int bounds (the interpreter raises TypeMismatch; the JIT bails). `..=` shifts
/// the exclusive upper to `b + 1`; `saturating_add` keeps the unmaterializable `i64::MAX` edge
/// from panicking. The elements are fresh int immediates (no refcount), so no retain is needed.
#[inline(always)]
pub(crate) fn make_range_list(lo: Value, hi: Value, inclusive: bool) -> Option<Value> {
    let (a, b) = (lo.as_int()?, hi.as_int()?);
    let upper = if inclusive { b.saturating_add(1) } else { b };
    let elements: Vec<Value> = (a..upper).map(Value::int).collect();
    Some(Value::list(elements))
}

/// `Op::IterSnapshot`'s happy path for a non-object source: a packed list materializes directly
/// into an owned boxed snapshot; a list's elements, a set's canonical elements, or a map's values
/// in key order are snapshotted with each element retained, so the loop owns them independently.
/// `None` = not iterable (the interpreter raises; the JIT bails). The caller handles the
/// `Iterable::iter` object dispatch before calling.
/// `Op::IterSnapshot`'s happy path under the loop's ordering hint: a `Set`/`Map` whose element or key type
/// carries a `u64` hands the loop its elements in the order the *type* states rather than the erased
/// word's. The collection itself is untouched — this reorders the snapshot the loop walks, never the
/// set's canonical buffer or the map's key placement, which are identity orders (see
/// [`noeta_ast::render_hint`]). A `List` is never reordered: its order is its data.
#[inline(always)]
pub(crate) fn iter_snapshot_value_hinted(
    v: Value,
    hint: Option<&noeta_ast::RenderHint>,
) -> Option<Value> {
    if v.is_packed_list() {
        return Some(v.realize_list());
    }
    let elements = match hint {
        // A map's values, re-ordered by the OBSERVED key order.
        Some(hint) if v.is_map() => {
            let mut entries = v.map_entries_keyed()?;
            let key = hint.entry_key();
            entries.sort_by(|a, b| noeta_ast::map_key_order(&a.0, &b.0, key));
            entries.into_iter().map(|(_, value)| value).collect()
        }
        Some(hint) if v.is_set() => {
            let mut items = v.set_items()?;
            let elem = hint.elements();
            items.sort_by(|&a, &b| {
                noeta_value::compare_values_hinted(a, b, elem).unwrap_or(std::cmp::Ordering::Equal)
            });
            items
        }
        _ => v
            .list_items()
            .or_else(|| v.set_items())
            .or_else(|| v.map_values())?,
    };
    for &e in &elements {
        retain(e);
    }
    Some(Value::list(elements))
}

/// `Op::ListGet`'s happy path: a non-negative int index in bounds → the element, owned by the
/// caller (retained here). `None` = non-int/negative/out-of-bounds — unreachable for the
/// loop-generated op (the interpreter asserts; the JIT bails). The source is always a boxed list
/// here (`IterSnapshot` materialized any packed form).
#[inline(always)]
pub(crate) fn list_get_retained(list: Value, index: Value) -> Option<Value> {
    let element = index
        .as_int()
        .filter(|&i| i >= 0)
        .and_then(|i| list.list_get(i as usize))?;
    retain(element);
    Some(element)
}

/// `Op::Index`'s list happy path, bounds already checked: a packed list materializes the one
/// indexed element (owned, refcount 1) — no full-list materialization, no extra retain; a boxed
/// list borrows the element and retains it into the destination.
#[inline(always)]
pub(crate) fn list_element_retained(v: Value, i: usize) -> Value {
    if v.is_packed_list() {
        v.packed_get(i)
    } else {
        let element = v.list_get(i).expect("bounds checked by the caller");
        retain(element);
        element
    }
}

/// `Op::MakeTuple` (no failure path — construction never fails): retain each element register
/// into a fresh tuple. The retains land in the local vector, then the tuple owns them.
#[inline(always)]
pub(crate) fn make_tuple(items: &[u16], regs: &[Value], base: usize) -> Value {
    let mut elements = Vec::with_capacity(items.len());
    for &r in items.iter() {
        let v = regs[base + r as usize];
        retain(v);
        elements.push(v);
    }
    Value::tuple(elements)
}

/// `Op::TupleIndex`'s happy path: positional projection `receiver.N`, retaining the element for
/// the caller. `None` = out of range / not a tuple (unreachable for well-typed code — the
/// interpreter raises; the JIT bails).
#[inline(always)]
pub(crate) fn tuple_element_retained(v: Value, index: usize) -> Option<Value> {
    let element = v.tuple_field(index)?;
    retain(element);
    Some(element)
}
