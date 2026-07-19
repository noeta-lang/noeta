//! The tier-0 **dispatch loop**: [`Vm::run`] (the frame-stack driver) and
//! [`Vm::dispatch`] — deliberately ONE function containing the whole op match
//! (splitting it was assessed and declined for jump-table codegen and
//! cohesion; see `plans/code-quality/split-vm-lib.md`) — plus its register
//! helpers (`set_reg`, `reserve_window`, [`ArgBuf`]). Moved verbatim from the
//! crate root purely to shrink `lib.rs` — no behavior change.

use crate::*;

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
        // Give the register stack generous headroom up front (P-JIT J3): a native direct call only
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
            // Phase 4.2c-ii: a panic unwinds the live frames. Before reclaiming their memory, fire
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
        // two vectors sized to the whole module's cache-slot count; the entries were cleared on the
        // previous exit, so a run still starts cold — the same fresh-per-run semantics as before.
        let (mut caches, mut extern_caches) = self.cache_pool.pop().unwrap_or_default();
        caches.resize(self.module.cache_slots as usize, None);
        // Extern-method route cache (H5 perf): per `CallMethod` site, the resolved routing for an
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
            // Re-read the module each frame transfer, NOT once per dispatch: a debug-console
            // fragment install ([`Vm::install_fragment`], tooling-unification T4) swaps
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
            // Hover purity chokepoint (T6): a hover fragment runs as a single wrapper frame; every
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
            // Tier-0/tier-1 dispatch (P-JIT). Only at a fresh frame entry (`pc == 0`): a return-pop
            // re-enters `'reload` with the caller's saved `pc > 0`, and an in-frame jump never leaves
            // the inner loop, so `pc == 0` is exactly "this frame is starting". A compiled prototype
            // may run the whole frame in native code; J0 always bails, so control falls straight
            // through to the interpreter below (byte-identical).
            // Fire at every frame `'reload`, not only fresh entries: after a native `Call`'s callee
            // returns, the interpreter re-enters the caller at its resume pc and native execution
            // picks up there (J3 resume-native). `entry_pc = pc` is 0 for a fresh frame or the saved
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
            // OSR back-edge trigger (P-JIT J5): a taken backward branch to `target` is a loop
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
                    #[cfg(feature = "jit")]
                    {
                        let _osr_t = $target as usize;
                        if _osr_t <= pc
                            && (self.tier1.jit.is_some() || self.tier1.jit_service.is_some())
                            && self.jit_osr_backedge(proto)
                        {
                            frames[top].pc = _osr_t;
                            continue 'reload;
                        }
                    }
                };
            }
            loop {
                // Profiler seam (`noeta profile`): before each instruction, let the attached profiler
                // observe the live stack (it diffs frame depth to detect call enter/exit, or samples
                // when a tick is pending). `None` on every non-profile run — one predicted branch. The
                // frame's `pc` is synced first so the view resolves the right current line. It never
                // pauses, so unlike the debugger it needs no take/restore: it borrows only the frame
                // stack + registers (dispatch params, not `self`) and `module` (a local reference).
                if let Some(prof) = self.profiler.as_mut() {
                    frames[top].pc = pc;
                    let view = DebugView {
                        module,
                        frames: &frames[..],
                        regs: &regs[..],
                    };
                    prof.before_op(&view);
                }
                // Debugger seam (`noeta dap`): before each instruction, let the attached debugger map
                // `(proto, pc)` to a source line and pause if a breakpoint/step/entry condition holds.
                // `None` on every non-debug run — one predicted branch. The frame's `pc` is synced
                // first so a paused stack trace reads the instruction about to run. `Terminate` (a
                // disconnect while paused) unwinds cleanly, releasing the stack like any abort.
                if self.debugger.is_some() {
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
                                // Every evaluate compiles through the adopted session (T5): full
                                // language for a watch/console, and for a hover the same engine
                                // gated to the read-only surface (T6) — one evaluator, not two. The
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
                            // A Variables-panel edit (U1): evaluate the replacement value as a
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
                                    };
                                    dbg.after_side_effect(&view);
                                }
                                let _ = reply.send(outcome);
                            }
                        }
                    }
                    self.debugger = Some(dbg);
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
                        // the drop destructor-relevant (Phase 4), route it through `release_value` so a
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
                        let result = if l.is_list() && r.is_list() {
                            if l.is_packed_list()
                                && r.is_packed_list()
                                && l.is_uniquely_owned()
                                && l.packed_extend_in_place(r)
                            {
                                // Sole owner, both flat, same layout: append `rhs`'s words to `lhs`'s
                                // buffer in place (P-PACK 2.6). The single reference moves into the result.
                                l
                            } else if !l.is_packed_list()
                                && !r.is_packed_list()
                                && l.is_uniquely_owned()
                            {
                                // Sole owner, both boxed: extend the backing buffer in place (O(1)
                                // amortized). The single reference moves from `lhs` into the result.
                                l.list_extend(r);
                                l
                            } else if let Some(flat) = l.packed_concat(r) {
                                // Aliased but both flat (same layout): copy the word buffers, then drop the
                                // consumed accumulator reference — stays flat without mutating the alias.
                                release(l);
                                flat
                            } else {
                                // A mixed packed/boxed pairing (or differing layouts): copy, preserving
                                // immutable semantics. Demote each operand to an owned boxed list, retain
                                // each element into the new list, release the demotions, then drop the
                                // accumulator's consumed reference.
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
                            // Sole owner of a string accumulator: append `rhs`'s display form to its
                            // buffer in place (amortized O(1)), mirroring the list path — the single
                            // reference moves into the result. This is what makes `s = s ~ x` in a loop
                            // O(n) instead of O(n²) (the `format!` below copies all of `l` each time).
                            l.str_push_in_place(&r.display());
                            l
                        } else {
                            // Aliased accumulator or non-string lhs: display concatenation into a fresh
                            // string (preserves immutable semantics), identical to `Op::Binary`'s `~`.
                            let s = Value::string(&format!("{}{}", l.display(), r.display()));
                            release(l);
                            s
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
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
                                CaptureFrom::Upvalue(index) => {
                                    frames[top].upvalues[*index as usize]
                                }
                            };
                            retain(cell);
                            upvalues.push(cell);
                        }
                        let v = Value::closure(*proto, upvalues);
                        set_reg(regs, fbase, *dst, v);
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
                        // Stamp the checker-resolved element type onto the list (R1) so `type_of` recovers
                        // it after a `dyn` launder. A cheap `Rc` clone of the shared load-time entry; the
                        // tag lives beside the payload, invisible to value semantics.
                        if let Some(idx) = reflect {
                            list.set_reflect(Some(Rc::clone(
                                &self.persist.type_reprs[*idx as usize],
                            )));
                        }
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    // A `List<packed>` literal (P-PACK 2.4): pack each element into a flat raw-primitive
                    // buffer (no boxed objects, no retains — the words are copied), then the element
                    // temporaries are released by the following compiler-emitted drops, exactly as for
                    // `MakeList`'s consumed operands. If any element fails to pack (a shape the schema
                    // does not expect — not reachable for a well-typed marked site), fall back to a boxed
                    // list that retains each element, staying consistent with those drops.
                    Op::PackedListNew { dst, schema } => {
                        // Allocate the empty flat buffer the following `PackedListPush` chain fills
                        // (P-PACK 2.5 streaming construction).
                        let schema = self.persist.packed_schemas[*schema as usize];
                        let list = Value::packed_list(schema, Vec::new());
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    Op::FromBytes {
                        dst,
                        src,
                        schema,
                        span,
                    } => {
                        // Deserialize a `bytes` buffer into a flat `List<T>` (P-PACK 4.4): wrap the raw
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
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    Op::TypedModuleCall {
                        dst,
                        module: mod_id,
                        func: func_id,
                        args,
                        recipe,
                        ok_shape,
                        err_shape,
                        span,
                    } => {
                        // Resolve the interned module/func names (`module` is the outer loop-local
                        // `&Module`, so bind the op's ids under different names to avoid shadowing it).
                        let mod_name = module.name(*mod_id);
                        let func = module.name(*func_id);
                        // The recipe is required; its absence was already reported by the checker.
                        let Some(recipe) = recipe else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`{mod_name}.{func}::<T>(...)` has no resolved result type"
                                ),
                            ));
                        };
                        // The call-site-typed native functions: `json.parse::<T>` (aborting) and the
                        // recoverable `json.decode::<T>` → `Result<T, string>` (L2 DI).
                        let recoverable = func == "decode";
                        if mod_name == "json" && (func == "parse" || recoverable) {
                            let text = args
                                .first()
                                .map(|r| regs[fbase + *r as usize])
                                .and_then(|v| v.as_string());
                            let Some(text) = text else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("`json.{func}` expects a `string` argument"),
                                ));
                            };
                            match noeta_stdlib::json::parse_typed(&text, recipe) {
                                Ok(out) => {
                                    let value = materialize_recipe(out);
                                    let value = if recoverable {
                                        Value::enum_value(
                                            self.persist.shapes[*ok_shape as usize],
                                            vec![value],
                                        )
                                    } else {
                                        value
                                    };
                                    set_reg(regs, fbase, *dst, value);
                                }
                                Err(error) if recoverable => {
                                    // A decode failure is a recoverable `Result.Err(message)`.
                                    let msg = Value::string(&error.message);
                                    let err = Value::enum_value(
                                        self.persist.shapes[*err_shape as usize],
                                        vec![msg],
                                    );
                                    set_reg(regs, fbase, *dst, err);
                                }
                                Err(error) => {
                                    return Err(self.error(
                                        stdlib_error_code(error.kind),
                                        *span,
                                        error.message,
                                    ));
                                }
                            }
                        } else {
                            return Err(self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!(
                                "`{mod_name}.{func}::<T>(...)` is not a call-site-typed native function"
                            ),
                        ));
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
                        // The router-facing runtime decode (L2.2 DI). Fully recoverable: an unknown type
                        // name, a non-string operand, or a malformed body all land as `Result.Err`; a
                        // good decode is `Result.Ok(value)` wrapping the materialized struct. Mirrors
                        // the recoverable `json.decode::<T>` branch above, but the recipe is looked up by
                        // runtime type name rather than baked at the call site.
                        let err = |vm: &Self, msg: String| {
                            Value::enum_value(
                                vm.persist.shapes[*err_shape as usize],
                                vec![Value::string(&msg)],
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
                        let value = match self.deserialize_recipes.get(&type_name) {
                            None => err(self, format!("unknown deserializable type `{type_name}`")),
                            Some(recipe) => match noeta_stdlib::json::parse_typed(&text, recipe) {
                                Ok(out) => Value::enum_value(
                                    self.persist.shapes[*ok_shape as usize],
                                    vec![materialize_recipe(out)],
                                ),
                                Err(error) => err(self, error.message),
                            },
                        };
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::BundleMethod {
                        dst,
                        recv,
                        module: mod_id,
                        bundle: bundle_id,
                        method: method_id,
                        args,
                        span,
                    } => {
                        // A bound method-bundle call (kernel-methods K3): the route was baked at
                        // compile time — straight to the bundle's shared ctx dispatch, receiver
                        // as slot 0. Values are copied out of the registers first (borrowed by
                        // the ctx seed; the seam owns the refcount discipline).
                        let mod_name = module.name(*mod_id);
                        let bundle_name = module.name(*bundle_id);
                        let method_name = module.name(*method_id);
                        let recv_value = regs[fbase + *recv as usize];
                        let mut arg_values = Vec::with_capacity(args.len());
                        for r in args.iter() {
                            arg_values.push(regs[fbase + *r as usize]);
                        }
                        let result = self.call_bundle_method(
                            mod_name,
                            bundle_name,
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
                    // A tuple builds exactly like a list (object-model slice 4): retain each element into
                    // the aggregate, which owns one reference to each.
                    Op::MakeTuple { dst, items } => {
                        let tuple = make_tuple(items, regs, fbase);
                        set_reg(regs, fbase, *dst, tuple);
                        pc += 1;
                    }
                    // Positional projection `receiver.N`: read the Nth element of the tuple, retaining it
                    // into `dst`. The index is in range by construction (the checker verified it).
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
                            // A duplicate key keeps the later value (M0 `BTreeMap` semantics); the
                            // displaced value loses its owner, so release it.
                            if let Some(pos) = map.iter().position(|(k, _)| *k == key) {
                                let (_, old) = map.remove(pos);
                                release(old);
                            }
                            map.push((key, value));
                        }
                        let map = Value::map_keyed(map);
                        // Stamp the checker-resolved `Map(K, V)` type onto the map (R1) so `type_of`
                        // recovers it after a `dyn` launder — the same node-tag path `MakeList` uses.
                        if let Some(idx) = reflect {
                            map.set_reflect(Some(Rc::clone(
                                &self.persist.type_reprs[*idx as usize],
                            )));
                        }
                        set_reg(regs, fbase, *dst, map);
                        pc += 1;
                    }
                    Op::RequireMapKey { reg, span } => {
                        let v = regs[fbase + *reg as usize];
                        let ok = v.is_string()
                            // P-PKEY S4: ints key maps (`float` stays excluded — NaN).
                            || v.as_int().is_some()
                            || (v.is_extern()
                                && v.with_extern(noeta_stdlib::map_key::extern_key_capable))
                            // P-PKEY: a key-capable `@packed` struct keys a map by content.
                            || v.shape().is_some_and(|s| s.key_capable);
                        if !ok {
                            let error = noeta_stdlib::map_key::map_key_error(v.type_name());
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                error.message,
                            ));
                        }
                        pc += 1;
                    }
                    Op::IterSnapshot { dst, src, span } => {
                        let v = regs[fbase + *src as usize];
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
                                *dst,
                                RetTransform::None,
                                pc + 1,
                            )?;
                            continue 'reload;
                        }
                        // Snapshot the elements to iterate: a packed list materializes into an owned
                        // boxed snapshot (so `ListLen`/`ListGet` never see the flat form); a list's
                        // elements, a set's canonical elements, or a map's values in sorted-key order
                        // are each retained so the loop owns them independently.
                        let Some(snapshot) = iter_snapshot_value(v) else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot iterate over {}", v.type_name()),
                            ));
                        };
                        set_reg(regs, fbase, *dst, snapshot);
                        pc += 1;
                    }
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
                                    *dst,
                                    RetTransform::None,
                                    pc + 1,
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
                        span,
                        cache,
                        reuse,
                        consume_key,
                    } => {
                        // Resolve the interned method name once; every path below wants the `&str`.
                        let method = module.name(*method);
                        let v = regs[fbase + *recv as usize];
                        // Classify the receiver once (one heap dereference). Every rung below
                        // tests `hk` with an integer compare instead of re-probing the heap
                        // per candidate type — a deep rung (map/iter methods) used to pay a
                        // dereference for every rung above it.
                        let hk = v.heap_kind();
                        // In-place map self-update (Phase 5.1c): a reuse-marked `m = m.set(k,v)` /
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
                        // `json.parse(...)` — a Ring 2 native module function call, dispatched before
                        // the object/collection paths.
                        if hk == Some(HeapKind::NativeModule)
                            && let Some(module_name) = v.native_module_name()
                        {
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            let value = self.call_native_module(
                                &module_name,
                                method,
                                arg_values.as_slice(),
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, value);
                            pc += 1;
                            continue;
                        }
                        // An extern receiver routes through the per-site cache (H5 perf): a
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
                                    let route = crate::methods::resolve_extern_route(
                                        self.reg(),
                                        identity,
                                        method,
                                    );
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
                                        let value = self.persist.ext_arena[retained as usize]
                                            .expect("a live arena entry");
                                        retain(value);
                                        set_reg(regs, fbase, *dst, value);
                                        pc += 1;
                                        continue;
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
                                let value = self.call_ctx_type_method(
                                    identity,
                                    v,
                                    method,
                                    arg_values.as_slice(),
                                    *span,
                                )?;
                                set_reg(regs, fbase, *dst, value);
                                pc += 1;
                                continue;
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
                                    pc += 1;
                                    continue;
                                }
                            }
                            // Inline cache: a hit (the receiver's shape pointer matches the cached one)
                            // gives the resolved prototype directly, skipping the `(type, method)` hashmap
                            // lookup and its two `String` clones. The hit check avoids bumping the shape
                            // refcount (raw pointer compare); only a miss clones the shape into the cache.
                            let ci = *cache as usize;
                            let shape_ptr = v.object_shape_ptr();
                            let hit = match &caches[ci] {
                                Some((cs, p))
                                    if Some(std::ptr::from_ref::<Shape>(cs)) == shape_ptr =>
                                {
                                    Some(*p)
                                }
                                _ => None,
                            };
                            let proto = match hit {
                                Some(proto) => proto,
                                None => {
                                    let shape = v.shape().unwrap();
                                    let Some(proto) = self.method_proto(&shape.name, method) else {
                                        return Err(self.error(
                                            DiagnosticCode::UnknownName,
                                            *span,
                                            format!(
                                                "type `{}` has no method `{method}`",
                                                shape.name
                                            ),
                                        ));
                                    };
                                    caches[ci] = Some((shape, proto));
                                    proto
                                }
                            };
                            let callee_chunk = &module.protos[proto as usize];
                            // The prototype takes the receiver in register 0 and the user arguments
                            // after it, so its declared arity is one more than the supplied args. A
                            // method may have trailing defaulted parameters, so the supplied count is a
                            // range `[total - defaults, total]` (all less the receiver).
                            let total = callee_chunk.num_params as usize - 1;
                            let required = total - callee_chunk.defaults.len();
                            if args.len() < required || args.len() > total {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    arity_message("method", required, total, args.len()),
                                ));
                            }
                            let arg_values = ArgBuf::collect(args, regs, fbase);
                            self.push_callee_frame(
                                frames,
                                regs,
                                top,
                                proto,
                                Some(v),
                                arg_values.as_slice(),
                                *dst,
                                RetTransform::None,
                                pc + 1,
                            )?;
                            continue 'reload;
                        }
                        // An enum value dispatches to a user method (the unified body, object-model
                        // slice 3) through the same type→method table as an object — and, audit-1
                        // finding 7, through the same per-site inline cache: an enum's `&'static
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
                            if method == "to_json"
                                && args.is_empty()
                                && self.tojson_derives.contains(&shape.name)
                            {
                                let json = Value::string(&v.to_json());
                                set_reg(regs, fbase, *dst, json);
                                pc += 1;
                                continue;
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
                                let total = callee_chunk.num_params as usize - 1;
                                let required = total - callee_chunk.defaults.len();
                                if args.len() < required || args.len() > total {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        arity_message("method", required, total, args.len()),
                                    ));
                                }
                                let arg_values = ArgBuf::collect(args, regs, fbase);
                                self.push_callee_frame(
                                    frames,
                                    regs,
                                    top,
                                    proto,
                                    Some(v),
                                    arg_values.as_slice(),
                                    *dst,
                                    RetTransform::None,
                                    pc + 1,
                                )?;
                                continue 'reload;
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
                                *dst,
                                RetTransform::None,
                                pc + 1,
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
                                    // Not a string: an int keys directly (P-PKEY S4), a
                                    // key-capable extern value probes through the contract
                                    // (extern-types X4), a key-capable packed struct by its
                                    // content snapshot (P-PKEY); anything else is the existing
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
                        // materialization (the P-PACK 2.5+ scalar-access win). Any miss (non-int index,
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
                        // A slot still unset after spread + named is filled from its field default
                        // (slice 5), run in global scope (empty upvalues — a default resolves globals
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
                        // Stamp the reflected type onto a generic instantiation (R2) so `type_of` recovers
                        // its type arguments after a `dyn` launder. The object's type is invariant under
                        // field mutation, so — unlike the collection tags — it is never cleared.
                        if let Some(idx) = reflect {
                            object.set_reflect(Some(Rc::clone(
                                &self.persist.type_reprs[*idx as usize],
                            )));
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
                            // Reuse keeps the base node's existing reflected type (R2): a self-update
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
                        // Stamp the reflected type onto a generic enum-variant construction (R2b.2) so
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
                        let key = match regs[fbase + *arg as usize].as_string() {
                            Some(s) => s,
                            None => {
                                let kind = if *panic { "from" } else { "try_from" };
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`{enum_name}.{kind}` expects a string, found {}",
                                        regs[fbase + *arg as usize].type_name()
                                    ),
                                ));
                            }
                        };
                        let matched = cases.iter().find(|(name, _)| module.name(*name) == key);
                        let result = match matched {
                            Some((_, shape_idx)) => {
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
                                return Err(self.error(
                                    DiagnosticCode::Panic,
                                    *span,
                                    format!("panic: `{enum_name}` has no case `{key}`"),
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
                        // leaf helper (P-JIT J4); a `false` return is the field-not-found error path.
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
                            // as `Op::Return` does (the M0 `Unwind::Return`).
                            Some(TryOutcome::Empty) => {
                                retain(v);
                                // Drop the frame locals this `?` abandons before unwinding (Phase 4.2c) —
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
                                let n =
                                    module.protos[finished.proto as usize].num_registers as usize;
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
                                    None => return Ok(out),
                                }
                                // `?` short-circuits like an early return — re-derive the caller's window.
                                continue 'reload;
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
                        some_shape,
                        none_shape,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let result = if narrow_matches(v, target) {
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
                    Op::IsType { dst, src, target } => {
                        let v = regs[fbase + *src as usize];
                        let result = Value::bool(narrow_matches(v, target));
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
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
                        let future =
                            std::mem::replace(&mut regs[fbase + *src as usize], Value::unit());
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
                        // Open a structured-concurrency scope (Track A.3b): a fresh, empty task list.
                        self.sched.scopes.push(Vec::new());
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
                            let scope_idx = self.sched.scopes.len() - 1;
                            let task_idx = self.sched.scopes[scope_idx].len();
                            // The child inherits a snapshot of the spawner's task-local context
                            // (T5a): a task spawned inside `with_span` parents its spans there.
                            let context = self.sched.ctx_current.clone();
                            self.sched.scopes[scope_idx].push(Task {
                                future,
                                result: None,
                                cancelled: false,
                                polling: false,
                                context,
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
                        // Join the scope (drive every task to completion), then pop it and release the
                        // tasks' owned futures and results.
                        self.join_scope(*span, Some((frames, regs)))?;
                        if let Some(scope) = self.sched.scopes.pop() {
                            for task in scope {
                                // Destructor-aware: a task's future holds the async body's captured
                                // locals in its state-machine cells. A completed task's cells are spent,
                                // but a **cancelled** task (a `race` loser) abandoned its future mid-body
                                // with a live captured value — release it here so its destructor runs.
                                self.release_value(task.future);
                                if let Some(result) = task.result {
                                    self.release_value(result);
                                }
                            }
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
                    Op::AttributesOf { dst, type_name } => {
                        let result = self.materialize_attributes(module.name(*type_name));
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::RolesOf { dst, role_enum } => {
                        let filter = role_enum.map(|e| module.name(e));
                        let result = self.materialize_roles(filter);
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
                    Op::TypeOf { dst, src } => {
                        let repr = vm_type_repr(&regs[fbase + *src as usize]);
                        let result = build_type_value(&repr);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::FieldsOf { dst, src } => {
                        let result = self.materialize_fields(regs[fbase + *src as usize]);
                        set_reg(regs, fbase, *dst, result);
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
                        let value =
                            build_type_value(&module.reflection.type_ref_repr(module.name(*name)));
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
                        ..
                    } => {
                        let recv_val = regs[fbase + *recv as usize];
                        let name_val = regs[fbase + *name as usize];
                        let args_val = regs[fbase + *args as usize];
                        // A packed args list (P-PACK 2.4) is materialized to a temporary boxed list for
                        // the duration of the dispatch, then released after the call frame is built (its
                        // elements retained into it). `arg_items` below borrows from this temporary.
                        let mut args_to_release: Option<Value> = None;
                        // Resolve the dispatch by name: either a prototype to call (`Ok`) or a reason it
                        // failed (`Err(msg)` → `Result.Err`). Every resolution failure — non-string name,
                        // non-list args, non-invokable receiver, unknown name, arity mismatch — is a
                        // runtime `Err`, never an abort (only a panic *inside* the called body aborts).
                        let outcome: Result<(u32, bool, Vec<Value>), String> = 'resolve: {
                            let Some(method) = name_val.as_string() else {
                                break 'resolve Err(format!(
                                    "invoke name must be a string, found {}",
                                    name_val.type_name()
                                ));
                            };
                            if !args_val.is_list() {
                                break 'resolve Err(format!(
                                    "invoke args must be a list, found {}",
                                    args_val.type_name()
                                ));
                            }
                            let args_list = args_val.realize_list();
                            args_to_release = Some(args_list);
                            let arg_items = args_list.list_items().expect("checked is_list");
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
                                "associated function"
                            } else {
                                "method"
                            };
                            let Some(proto) = self.method_proto(&type_name, &method) else {
                                break 'resolve Err(format!(
                                    "type `{type_name}` has no {kind} `{method}`"
                                ));
                            };
                            // The prototype reserves register 0 for `self` (unit for an associated
                            // call), so its declared arity is one more than the supplied args; trailing
                            // defaults widen the accepted range, exactly as `Op::CallMethod`.
                            let callee_chunk = &module.protos[proto as usize];
                            let total = callee_chunk.num_params as usize - 1;
                            let required = total - callee_chunk.defaults.len();
                            if arg_items.len() < required || arg_items.len() > total {
                                break 'resolve Err(arity_message(
                                    kind,
                                    required,
                                    total,
                                    arg_items.len(),
                                ));
                            }
                            Ok((proto, is_assoc, arg_items))
                        };
                        match outcome {
                            Err(message) => {
                                let shape = self.persist.shapes[*err_shape as usize];
                                let err = Value::enum_value(shape, vec![Value::string(&message)]);
                                set_reg(regs, fbase, *dst, err);
                                pc += 1;
                            }
                            Ok((proto, is_assoc, arg_items)) => {
                                // An associated call leaves register 0 as unit (no receiver); an instance
                                // call places the retained receiver there. The result is wrapped in
                                // `Result.Ok` as it lands in the caller, so the invocation yields a
                                // `Result` whichever way the body returns.
                                let recv = (!is_assoc).then_some(recv_val);
                                let ok = self.persist.shapes[*ok_shape as usize];
                                self.push_callee_frame(
                                    frames,
                                    regs,
                                    top,
                                    proto,
                                    recv,
                                    &arg_items,
                                    *dst,
                                    RetTransform::WrapOk(ok),
                                    pc + 1,
                                )?;
                                // Release the temporary boxed args list before transferring (its
                                // elements were already retained into the call frame above); `take`
                                // leaves the after-match release for the non-transferring `Err` path.
                                if let Some(list) = args_to_release.take() {
                                    list.release();
                                }
                                continue 'reload;
                            }
                        }
                        // Release the temporary boxed args list (if the args were materialized from a
                        // packed list); its elements were retained into the call frame above.
                        if let Some(list) = args_to_release {
                            list.release();
                        }
                    }
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
                        let matches = v.is_enum()
                            && v.shape().is_some_and(|shape| {
                                shape.variant.as_deref() == Some(module.name(*variant))
                                    && type_name
                                        .is_none_or(|t| module.name(t) == shape.name.as_str())
                            })
                            && v.enum_data().is_some_and(|d| d.len() == *arity as usize);
                        if matches {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    // A tuple pattern test (object-model slice 4b.2): `src` must be a tuple of exactly
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
                        let element =
                            regs[fbase + *src as usize].enum_data().unwrap()[*index as usize];
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
                                // `..xs` (spread) returns the source value unchanged, so the result
                                // aliases a live heap reference — retain it before `set_reg` releases
                                // the old occupant of `dst` (which is `src`). A no-op for the fresh
                                // primitives `Neg`/`Not` produce; mirrors `Op::Move`.
                                retain(v);
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
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
                        // in-body `impl` blocks are uniform across kinds — object-model slice 3): an
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
                                *dst,
                                transform,
                                pc + 1,
                            )?;
                            continue 'reload;
                        }
                        // Derived structural comparison: `< <= > >=` on an object or enum whose
                        // type `@derive(Comparable)`s (and has no hand-written `compare`) —
                        // field-wise ordering for objects, variant-declaration-index then payload
                        // for enums, computed synchronously (no method to call).
                        if (left.is_object() || left.is_enum())
                            && op.comparable_method().is_some()
                            && self
                                .comparable_derives
                                .contains(&left.shape().unwrap().name)
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
                        match apply_binary(*op, left, right) {
                            Ok(v) => {
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
                        }
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
                        // Sign-dependent fixed-width op (Tier W3): `/ % < <= > >=` on erased-int operands,
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
                        // Width-exact bit intrinsic (Tier W5): compute within `bits`, not the erased i64.
                        // The checker guarantees an integer receiver and (for `rotate_*`) an integer arg.
                        let recv_int = regs[fbase + *recv as usize].as_int().unwrap_or(0);
                        let amount = match arg {
                            Some(r) => regs[fbase + *r as usize].as_int().unwrap_or(0),
                            None => 0,
                        };
                        let value = Value::int(noeta_stdlib::int_method_width(
                            recv_int, *method, amount, *bits,
                        ));
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
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
                        self.out.stdout.push_str(&text);
                        self.out.stdout.push('\n');
                        pc += 1;
                    }
                    Op::Stringify { dst, src, span } => {
                        let v = regs[fbase + *src as usize];
                        // A user object or enum value lights up the `Display` trait: render it via its
                        // `to_string` method (which runs bytecode, so it is pushed as a call frame). The
                        // method table is keyed by the value's shape name, identical for both kinds
                        // (object-model slice 3). Matches the tree-walker's `display_value`.
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
                                *dst,
                                RetTransform::None,
                                pc + 1,
                            )?;
                            continue 'reload;
                        }
                        // Identity for every other value: the consuming `Echo`/`Concat` stringifies
                        // it via `display`.
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::BuildString { dst, parts } => {
                        // One pass, one output allocation (P-VMT-STR). Size the buffer from the
                        // literal segments (known up front); holes grow it as they render. Each hole
                        // register holds an already-`Stringify`-ed value (a `Display` object was
                        // dispatched to `to_string` by the preceding `Stringify`), so `display` here
                        // never pushes a frame — the whole build stays within this one op. Holes are
                        // read by value (`Value` is `Copy`); their registers keep ownership and are
                        // released at frame teardown, exactly as the old fold's temporaries were.
                        let cap: usize = parts
                            .iter()
                            .map(|p| match p {
                                StrPart::Literal(k) => match &chunk.consts[*k as usize] {
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
                                    if let Const::Str(s) = &chunk.consts[*k as usize] {
                                        out.push_str(s);
                                    }
                                }
                                StrPart::Hole(r) => {
                                    // Render directly into the buffer — no per-hole `display()` clone.
                                    regs[fbase + *r as usize].display_into(&mut out);
                                }
                            }
                        }
                        // Move the finished buffer into the heap string — no second copy.
                        set_reg(regs, fbase, *dst, Value::from_string(out));
                        pc += 1;
                    }
                    Op::Raise { idx } => {
                        self.out
                            .diagnostics
                            .push(chunk.diagnostics[*idx as usize].clone());
                        return Err(Abort);
                    }
                    Op::Call {
                        dst,
                        callee,
                        args,
                        span,
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
                            *span,
                            pc + 1,
                        )? {
                            continue 'reload;
                        }
                        pc += 1;
                    }
                    Op::CallGlobal {
                        dst,
                        global,
                        args,
                        span,
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
                            *span,
                            pc + 1,
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
                }
            }
        }
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
        ret_dst: u16,
        ret_transform: RetTransform,
        resume_pc: usize,
    ) -> Result<(), Abort> {
        let module = self.module;
        let callee_chunk = &module.protos[proto as usize];
        let new_base = reserve_window(regs, callee_chunk.num_registers as usize);
        if let Some(r) = recv {
            retain(r);
            regs[new_base] = r;
        }
        for (i, &a) in args.iter().enumerate() {
            retain(a);
            regs[new_base + i + 1] = a;
        }
        // Fill any omitted trailing parameters from their default thunks. The receiver slot and
        // supplied args occupy registers `0..=args.len()`, so a default register at or beyond
        // that was not supplied.
        if !callee_chunk.defaults.is_empty() {
            let defaults = callee_chunk.defaults.clone();
            let filled = args.len() + 1;
            for (reg, dproto) in &defaults {
                if *reg as usize >= filled {
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
enum ArgBuf {
    Inline([Value; ArgBuf::INLINE], usize),
    Heap(Vec<Value>),
}

impl ArgBuf {
    const INLINE: usize = 8;

    /// Copy the argument registers out of the frame window. The registers keep ownership
    /// (arguments are borrowed by every consumer), exactly as the `Vec` collect did.
    #[inline]
    fn collect(args: &[Reg], regs: &[Value], base: usize) -> Self {
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
    fn as_slice(&self) -> &[Value] {
        match self {
            ArgBuf::Inline(buf, n) => &buf[..*n],
            ArgBuf::Heap(v) => v,
        }
    }
}

/// Overwrite a register, releasing the value it held.
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
// cases and `Op::LoadField` (interpreter-side inline cache, measured not-profitable for tier 1 in
// J6) — stays at its call sites with the divergence documented there.

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
/// in sorted-key order are snapshotted with each element retained, so the loop owns them
/// independently. `None` = not iterable (the interpreter raises; the JIT bails). The caller
/// handles the `Iterable::iter` object dispatch before calling.
#[inline(always)]
pub(crate) fn iter_snapshot_value(v: Value) -> Option<Value> {
    if v.is_packed_list() {
        return Some(v.realize_list());
    }
    let elements = v
        .list_items()
        .or_else(|| v.set_items())
        .or_else(|| v.map_values())?;
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
