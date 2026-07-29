//! The **tier-1 (JIT) runtime glue** (P-JIT): the `extern "C"` helper symbols
//! generated code calls back into, [`frame_layout`] / `fresh_frame_template` /
//! [`compile_module_aot`], [`PreparedCall`] + the call/return trampolines, and
//! the `impl Vm` engine management (`init_jit` / `init_jit_service` /
//! `jit_enter` / `jit_osr_backedge` / `jit_install`). Everything is
//! `#[cfg(feature = "jit")]` / `"jit-rt"`-gated exactly as it was at the crate
//! root; the module itself is declared unconditionally and compiles to nothing
//! without those features. Moved verbatim purely to shrink `lib.rs` — no
//! behavior change.

use crate::*;

/// What a compiled prototype's tier-1 run tells the interpreter to do next (P-JIT, decoded from the
/// [`noeta_jit_abi::CompiledFn`] `i64` return).
#[cfg(feature = "jit-rt")]
pub(crate) enum JitOutcome {
    /// Resume interpreting this frame at the given bytecode pc (the native code bailed there).
    Bail(usize),
    /// A native `Call` pushed a callee frame; re-derive the top frame and run it (`continue 'reload`).
    Called,
    /// A native `Return` transferred its result to the caller and popped this frame; re-derive the
    /// caller frame and continue (`continue 'reload`).
    Returned,
    /// The bottom frame returned natively; the run is over — yield its value (on `vm.jit_ret`).
    Halted,
    /// The frame aborted (a diagnostic is recorded); propagate the unwind.
    Abort,
}

// Counts how many times a tier-1 bail stub has called `jit_observe` on this thread — the J0 proof
// that generated native code actually ran (and reached a runtime helper), used by the tests.
#[cfg(feature = "jit-rt")]
thread_local! {
    static JIT_OBSERVE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The J0 runtime-helper skeleton (P-JIT): the generated bail stub calls this once per frame entry,
/// proving a compiled prototype can reach a Rust helper with the live VM pointer under the tier-1
/// ABI. It only bumps a thread-local counter here; J1+ registers the real `retain`/`release`/`call`
/// helpers beside it and reconstitutes `&mut Vm` from `vm` to service them.
#[cfg(feature = "jit-rt")]
#[cfg_attr(
    feature = "aot",
    allow(unsafe_code),
    unsafe(export_name = "noeta_jit_observe")
)]
extern "C" fn jit_observe(_vm: *mut core::ffi::c_void) {
    JIT_OBSERVE_COUNT.with(|c| c.set(c.get().wrapping_add(1)));
}

/// This thread's running total of tier-1 bail-stub entries (see [`jit_observe`]). Test-only: the
/// J0 proof that native code actually ran.
#[cfg(all(test, feature = "jit"))]
pub(crate) fn jit_observe_count() -> u64 {
    JIT_OBSERVE_COUNT.with(|c| c.get())
}

/// Runtime helper for native `StoreGlobal` (P-JIT globals): the compiled code has already written the
/// slot; this records `g` in `global_order` so program-end teardown destroys globals in reverse
/// binding order (the one part of a first-time `StoreGlobal` that can't be inlined — a `Vec` push may
/// reallocate). Called only on the unbound→bound transition, matching the interpreter's `None` arm.
///
/// # Safety
/// `vm` must be the live `*mut Vm` the tier-1 ABI passed; the call happens synchronously inside
/// `jit_enter`, where no other borrow of `*vm` is active.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_note_global_bound"))]
extern "C" fn jit_note_global_bound(vm: *mut core::ffi::c_void, g: u32) {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    vm.persist.global_order.push(g);
}

/// Runtime helper: bump a value's refcount (heap-aware register moves, J3). No-op on an immediate.
#[cfg(feature = "jit-rt")]
#[cfg_attr(
    feature = "aot",
    allow(unsafe_code),
    unsafe(export_name = "noeta_jit_retain")
)]
extern "C" fn jit_retain(v: u64) {
    retain(Value::from_bits(v));
}

/// Runtime helper: float `%` (S2). Rust's `%` on f64 **is** fmod, so tier parity holds by
/// construction; a NaN result is canonicalized by the caller's `box_float_and_store`, exactly as
/// the other float ops.
#[cfg(feature = "jit-rt")]
#[cfg_attr(
    feature = "aot",
    allow(unsafe_code),
    unsafe(export_name = "noeta_jit_fmod")
)]
extern "C" fn jit_fmod(a: f64, b: f64) -> f64 {
    a % b
}

/// Runtime helper: drop one reference to a value — the plain, non-destructor release the
/// interpreter's `set_reg` uses on an overwritten register (J3). No-op on an immediate.
#[cfg(feature = "jit-rt")]
#[cfg_attr(
    feature = "aot",
    allow(unsafe_code),
    unsafe(export_name = "noeta_jit_release")
)]
extern "C" fn jit_release(v: u64) {
    release(Value::from_bits(v));
}

/// Runtime helper: the destructor-aware release for an IR-relevant `Drop` (may run a `destruct`
/// block if this is the last reference), J3.
///
/// # Safety
/// `vm` must be the live `*mut Vm` the tier-1 ABI passed (see [`jit_note_global_bound`]).
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_release_value"))]
extern "C" fn jit_release_value(vm: *mut core::ffi::c_void, v: u64) {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    vm.release_value(Value::from_bits(v));
}

/// The layout of [`Frame`] and the `Vec` header — the single source of truth the JIT bakes into its
/// native call-frame codegen (P-CALL). Filled from `offset_of!`/`size_of!` on *this build's* `Frame`
/// and a one-time `Vec`-header probe; because the JIT compiles in the same process/build, the numbers
/// it bakes always match the real layout (a lock test asserts each offset locates its field). See
/// [`noeta_jit_abi::FrameLayout`].
#[cfg(feature = "jit")]
pub fn frame_layout() -> noeta_jit_abi::FrameLayout {
    let (vec_ptr_word, vec_len_word, vec_cap_word) = vec_header_words();
    noeta_jit_abi::FrameLayout {
        frame_size: size_of::<Frame>(),
        frame_align: align_of::<Frame>(),
        proto_offset: core::mem::offset_of!(Frame, proto),
        base_offset: core::mem::offset_of!(Frame, base),
        pc_offset: core::mem::offset_of!(Frame, pc),
        ret_dst_offset: core::mem::offset_of!(Frame, ret_dst),
        ret_transform_offset: core::mem::offset_of!(Frame, ret_transform),
        upvalues_offset: core::mem::offset_of!(Frame, upvalues),
        vec_ptr_word,
        vec_len_word,
        vec_cap_word,
    }
}

/// The zero-initialized [`Frame`] the JIT bakes its call-frame push from (P-CALL): every field at its
/// resting value (`proto`/`base`/`pc`/`ret_dst` = 0, `ret_transform` = `None`, `upvalues` = empty
/// `Vec`). The native frame-push codegen reads this template's *words* — not its address — and bakes
/// them as position-independent immediates (L3.1a audit), so the same literal produces byte-identical
/// codegen in the runtime JIT and the AOT object, and a bound native body writes a valid initial
/// `Frame` into any VM's frame stack. Shared by [`Vm::init_jit`], [`Vm::init_jit_service`], and the
/// AOT [`compile_module_aot`].
#[cfg(feature = "jit")]
fn fresh_frame_template() -> Box<Frame> {
    Box::new(Frame {
        proto: 0,
        base: 0,
        pc: 0,
        ret_dst: 0,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    })
}

/// Ahead-of-time compile **every** eligible prototype of `module` to a relocatable **object file**
/// (P-AOT L3.2b): the same native codegen as the runtime JIT, emitted into a host object
/// (ELF/Mach-O/COFF) with the [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`] dispatch table, instead of
/// finalized to executable pages. Returns the object bytes for `noeta build --native` to link
/// against the AOT runtime staticlib.
///
/// This lives in `noeta-vm` (not the CLI) because only this crate knows the [`Frame`] layout: the
/// object bakes the [`fresh_frame_template`] words as immediates, so it must be built from the exact
/// same template the runtime uses. The template is read during `compile_module` and needs to outlive
/// only that call, so a local box suffices.
#[cfg(feature = "jit")]
pub fn compile_module_aot(module: &Module) -> Result<Vec<u8>, String> {
    let template = fresh_frame_template();
    let template_ptr = template.as_ref() as *const Frame as *const u8;
    let mut jit = noeta_jit::Jit::new_object("noeta_aot", frame_layout(), template_ptr)?;
    jit.compile_module(module)?;
    jit.finish()
}

/// Identify which of a `Vec`'s three pointer-sized words hold its data pointer, length, and capacity,
/// by constructing a `Vec` with distinct, recognizable values and reading its raw words. `Vec<T>`'s
/// header layout is `T`-independent, so a `Vec<usize>` stands in for `Vec<Frame>`/`Vec<Value>`.
///
/// # Safety
/// `transmute_copy` reads the three header words of a live `Vec` by value; it neither moves nor frees
/// the `Vec`, and `size_of::<Vec<_>>() == size_of::<[usize; 3]>()`.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
fn vec_header_words() -> (usize, usize, usize) {
    let mut v: Vec<usize> = Vec::with_capacity(97);
    v.extend_from_slice(&[0usize; 5]); // len = 5
    let ptr = v.as_ptr() as usize;
    let len = v.len(); // 5
    let cap = v.capacity(); // >= 97
    // ptr (a heap address), len (5), and cap (>= 97) are pairwise distinct, so each word is uniquely
    // identifiable by value.
    let words: [usize; 3] = unsafe { core::mem::transmute_copy(&v) };
    let find = |target: usize| {
        words
            .iter()
            .position(|&w| w == target)
            .expect("Vec header word not found — layout probe failed")
    };
    (find(ptr), find(len), find(cap))
}

/// Runtime helper for a native `Op::Call` (J3): read the call back from `proto`/`pc` and run the
/// shared closure-call setup on the interpreter's frame/register stacks (pushing the callee frame or
/// completing a synchronous first-class-builtin call). Returns the [`noeta_jit`] outcome the compiled
/// function propagates: `OUTCOME_CALLED` (frame pushed), a resume pc (synchronous call done, continue
/// there), or `OUTCOME_ABORTED` (a diagnostic was recorded).
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed; the call runs
/// synchronously inside `jit_enter`, where no other borrow of them is active.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_call"))]
extern "C" fn jit_call(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
) -> i64 {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    // `emit_call` emits this helper for `Op::Call` *and* `Op::CallGlobal`; source the callee from a
    // register (Call) or straight from its global slot (CallGlobal — a known top-level `fn`, read
    // without a retain, exactly like the interpreter arm).
    let (dst, callee_val, args, type_args, span, supplied) =
        match &module.protos[proto as usize].code[pc as usize] {
            Op::Call {
                dst,
                callee,
                args,
                type_args,
                span,
                supplied,
            } => (
                *dst,
                regs[base + *callee as usize],
                args,
                type_args,
                *span,
                *supplied,
            ),
            Op::CallGlobal {
                dst,
                global,
                args,
                type_args,
                span,
                supplied,
            } => {
                let cv = vm.persist.globals[global.0 as usize];
                if cv.is_unbound() {
                    let msg = format!(
                        "cannot find `{}` in this scope",
                        module.global_name(*global)
                    );
                    let _ = vm.error(DiagnosticCode::UnknownName, *span, msg);
                    return noeta_jit_abi::OUTCOME_ABORTED;
                }
                (*dst, cv, args, type_args, *span, *supplied)
            }
            // `emit_call` only emits this helper for a call op, so this is unreachable; treat a
            // mismatch defensively as an abort rather than misbehave.
            _ => return noeta_jit_abi::OUTCOME_ABORTED,
        };
    let caller_top = frames.len() - 1;
    match vm.setup_closure_call(
        frames,
        regs,
        caller_top,
        base,
        dst,
        callee_val,
        args,
        type_args.regs(),
        span,
        pc as usize + 1,
        noeta_bytecode::supplied_of(supplied),
    ) {
        Ok(true) => noeta_jit_abi::OUTCOME_CALLED,
        Ok(false) => pc as i64 + 1,
        Err(Abort) => noeta_jit_abi::OUTCOME_ABORTED,
    }
}

/// Runtime helper for a native `Op::Return` (J3): run the shared return protocol (transfer the value
/// to the caller's destination, pop this frame). Returns `OUTCOME_RETURNED` when it transferred to a
/// caller, or `OUTCOME_HALTED` (parking the value on `vm.jit_ret`) when the bottom frame returned.
///
/// `release_mask` is the P-JSSA S4.0 fast teardown: bit `r` set means window slot `r` may hold a
/// heap value at this return site (the bare-store analysis row at the `Return`'s pc), so only
/// those slots need a release; `u64::MAX` means "release every slot" (an unanalyzed prototype, or
/// one with more than 64 registers). The mask is native-path-sound: this helper is reached only
/// by natively-executed `Op::Return`s, and native execution maintains the analysis's claims
/// (entries verify them, native defs preserve them) — a clear bit is a guarantee the slot holds
/// an immediate, whose release is a no-op.
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_return"))]
extern "C" fn jit_return(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    raw: u64,
    release_mask: u64,
) -> i64 {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    match vm.do_return_masked(frames, regs, Value::from_bits(raw), release_mask) {
        Some(v) => {
            vm.tier1.jit_ret = v;
            noeta_jit_abi::OUTCOME_HALTED
        }
        None => noeta_jit_abi::OUTCOME_RETURNED,
    }
}

/// The two-word result of [`jit_prepare_call`], returned by value (rax:rdx under SysV; the JIT
/// declares the import with two `i64` returns, which lowers to the same registers — one helper
/// roundtrip instead of the former `prepare_call` + `callee_base` pair, P-JSSA S4.0).
#[cfg(feature = "jit-rt")]
#[repr(C)]
struct PreparedCall {
    /// The callee's compiled entry pointer, or `0` (fall back to `jit_call`).
    fnptr: i64,
    /// The callee's reserved window base (meaningful only when `fnptr != 0`).
    base: usize,
}

/// Runtime helper for a native direct call (J3 native→native): decide whether the `Op::Call` at
/// `pc` can be called directly and, if so, set up the callee frame on the shared stacks and return
/// the callee's compiled entry pointer plus its window base; otherwise a zero `fnptr` (the caller
/// falls back to `jit_call`). Direct-able means: a closure callee, plain arity (no defaults), no
/// upvalues, an already-compiled callee, and stack capacity for the callee window without a
/// reallocation (so the caller's register pointer stays valid across the indirect call).
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_prepare_call"))]
extern "C" fn jit_prepare_call(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
    site: *mut noeta_jit_abi::CallSiteCache,
) -> PreparedCall {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    // Direct-call setup for `Op::Call` or `Op::CallGlobal`; the callee comes from a register or its
    // global slot. An unbound `CallGlobal` slot falls back to `jit_call`, which raises the E0005.
    const FALLBACK: PreparedCall = PreparedCall { fnptr: 0, base: 0 };
    // A supplied-mask means the call skips a defaulted parameter, so the callee's prologue must run
    // default thunks — not this path, which copies arguments positionally into a full window. The
    // exact-arity check below already excludes such a call (a hole leaves a parameter unfilled);
    // this is the explicit statement of that, so relaxing the arity check cannot silently
    // reintroduce a misplaced-argument bug.
    //
    // A non-empty `type_args` is refused for exactly the same reason, and this is why every field
    // below is bound by name rather than swept under a `..`: this path copies `args` into the
    // callee window POSITIONALLY, so a type-argument channel it never read would not crash — it
    // would put the first value argument into `$ty0` and hand the body a wrong answer. `jit_call`
    // above already named its fields and so would have refused to compile; these two would not
    // have, which is the asymmetry `the_tier1_helpers_bind_every_field` exists to close.
    let (dst, callee_val, args) = match &module.protos[proto as usize].code[pc as usize] {
        Op::Call {
            dst,
            callee,
            args,
            type_args,
            span: _,
            supplied,
        } => {
            if supplied.is_some() || !type_args.is_empty() {
                return FALLBACK;
            }
            (*dst, regs[base + *callee as usize], args)
        }
        Op::CallGlobal {
            dst,
            global,
            args,
            type_args,
            span: _,
            supplied,
        } => {
            if supplied.is_some() || !type_args.is_empty() {
                return FALLBACK;
            }
            let cv = vm.persist.globals[global.0 as usize];
            if cv.is_unbound() {
                return FALLBACK;
            }
            (*dst, cv, args)
        }
        _ => return FALLBACK,
    };
    let Some(callee_proto) = callee_val.as_closure() else {
        return FALLBACK; // a first-class builtin / non-callable → fall back
    };
    let cc = &module.protos[callee_proto as usize];
    // Plain arity (no default-filling) and no upvalues — else the general setup path handles it.
    if args.len() != cc.num_params as usize || callee_val.closure_upvalue_count() != 0 {
        return FALLBACK;
    }
    let num_regs = cc.num_registers as usize;
    // The callee's window must fit without reallocating the register stack (which would dangle
    // the caller's pointer).
    if regs.len() + num_regs > regs.capacity() {
        return FALLBACK;
    }
    // Fast convention (P-JSSA S4.1): the callee has a frameless-window body — reserve the window
    // WITHOUT initializing it (the fast body normalizes it before the interpreter can ever see
    // it) and skip the argument copy/retain (the arguments travel as machine arguments, borrowed
    // from the caller's still-live registers). Bit 0 of the returned pointer tags the convention.
    // Lookups go through the VM's mirror tables (P-PAR S4) — empty when the JIT is off, and the
    // only tier-1 tables the mutator may read in service mode.
    if let Some(ff) = vm
        .tier1
        .jit_fast
        .get(callee_proto as usize)
        .copied()
        .flatten()
    {
        // S4.2: fill this call site's inline cache so the next call with the same callee pushes
        // the frame natively, without this helper. The cached closure is **pinned** (retained +
        // held on `jit_cache_pins` until teardown) so its bits can never be reused by another
        // object while cached; a site that sees a second distinct callee is poisoned instead
        // (megamorphic — the pin stays until teardown, bounding pins by site count).
        if !site.is_null() {
            let slot = unsafe { &mut *site };
            if slot[0] == noeta_jit_abi::SITE_EMPTY {
                retain(callee_val);
                vm.tier1.jit_cache_pins.push(callee_val);
                slot[1] = ff as u64;
                slot[2] = num_regs as u64;
                slot[3] = callee_proto as u64;
                slot[0] = callee_val.bits();
            } else if slot[0] != callee_val.bits() {
                slot[0] = noeta_jit_abi::SITE_POISON;
            }
        }
        let new_base = regs.len();
        // SAFETY: capacity was checked above, and `reserve_window` keeps the register stack's
        // entire capacity initialized (its growth path fills to capacity), so every element in
        // `..new_base + num_regs` has been written at some point — `set_len`'s contract.
        #[allow(clippy::uninit_vec)]
        unsafe {
            regs.set_len(new_base + num_regs);
        }
        let caller_top = frames.len() - 1;
        frames[caller_top].pc = pc as usize + 1;
        frames.push(Frame {
            proto: callee_proto,
            base: new_base,
            pc: 0,
            ret_dst: dst,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        });
        return PreparedCall {
            fnptr: ff as i64 | 1,
            base: new_base,
        };
    }
    // The classic direct path needs the callee's normal body compiled.
    let Some(f) = vm.jit_entry(callee_proto as usize) else {
        return FALLBACK;
    };
    // Set up the callee frame (like `setup_closure_call`'s closure arm, minus defaults/upvalues).
    let new_base = reserve_window(regs, num_regs);
    for (i, &arg_reg) in args.iter().enumerate() {
        let v = regs[base + arg_reg as usize];
        retain(v);
        regs[new_base + i] = v;
    }
    let caller_top = frames.len() - 1;
    frames[caller_top].pc = pc as usize + 1;
    frames.push(Frame {
        proto: callee_proto,
        base: new_base,
        pc: 0,
        ret_dst: dst,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    });
    PreparedCall {
        fnptr: f as usize as i64,
        base: new_base,
    }
}

/// Runtime helper: interpret a direct callee's outcome for its native caller (J3). `RETURNED` → the
/// caller continues in place (`OUTCOME_CONTINUE`, result already in its destination). Otherwise the
/// callee did not complete natively — a bail sets the (still-live) callee frame's pc so the
/// interpreter resumes it there, and the caller propagates `CALLED`; `CALLED`/`ABORTED` pass through.
///
/// # Safety
/// `frames` must be the live pointer the tier-1 ABI passed.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_after_call"))]
extern "C" fn jit_after_call(
    _vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    callee_outcome: i64,
) -> i64 {
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    match callee_outcome {
        noeta_jit_abi::OUTCOME_RETURNED => noeta_jit_abi::OUTCOME_CONTINUE,
        noeta_jit_abi::OUTCOME_CALLED => noeta_jit_abi::OUTCOME_CALLED,
        noeta_jit_abi::OUTCOME_ABORTED => noeta_jit_abi::OUTCOME_ABORTED,
        // A bail pc: the callee frame is still the top; point it at its resume pc so the interpreter
        // runs it there, and tell the caller a frame is pending (CALLED). (HALTED can't occur — a
        // direct callee always has a caller — so it also lands here defensively as a re-run.)
        bail_pc => {
            if let Some(top) = frames.last_mut() {
                top.pc = bail_pc.max(0) as usize;
            }
            noeta_jit_abi::OUTCOME_CALLED
        }
    }
}

/// Runtime helper for a native leaf heap/collection op (J4): run the `Op` at `proto`/`pc` — the
/// interpreter's exact arm, refcounts and all — on the shared register stack, and return
/// `OUTCOME_CONTINUE` when it completed. It handles only the non-dispatching, non-erroring path of
/// each op; a receiver that would dispatch (a user `Iterable`/`Index`) or a case that would raise
/// returns the op's own pc, so the interpreter re-runs it. Every early return happens **before** any
/// register write, so a re-run in the interpreter starts from clean state.
///
/// # Safety
/// `vm`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit-rt")]
#[allow(unsafe_code)]
#[cfg_attr(feature = "aot", unsafe(export_name = "noeta_jit_run_leaf_op"))]
extern "C" fn jit_run_leaf_op(
    vm: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
) -> i64 {
    // Reconstitute `&mut Vm` (some leaf ops, e.g. `SetField`, release displaced values through
    // `self`); `regs_vec` points at the dispatch loop's local register stack, disjoint from the VM.
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    let bail = pc as i64;
    match &module.protos[proto as usize].code[pc as usize] {
        Op::MakeRange {
            dst,
            start,
            end,
            inclusive,
            span: _,
        } => {
            let lo = regs[base + *start as usize];
            let hi = regs[base + *end as usize];
            let Some(list) = make_range_list(lo, hi, *inclusive) else {
                return bail; // non-int bounds → interpreter raises the error
            };
            set_reg(regs, base, *dst, list);
            noeta_jit_abi::OUTCOME_CONTINUE
        }
        Op::IterSnapshot { dst, src, span: _ } => {
            let v = regs[base + *src as usize];
            if v.is_object() {
                return bail; // `Iterable::iter` dispatch → interpreter
            }
            match iter_snapshot_value(v) {
                Some(snapshot) => {
                    set_reg(regs, base, *dst, snapshot);
                    noeta_jit_abi::OUTCOME_CONTINUE
                }
                None => bail, // not iterable → interpreter raises the error
            }
        }
        Op::ListLen { dst, src, span: _ } => match regs[base + *src as usize].list_len() {
            Some(n) => {
                set_reg(regs, base, *dst, Value::int(n as i64));
                noeta_jit_abi::OUTCOME_CONTINUE
            }
            None => bail,
        },
        Op::ListGet { dst, list, index } => {
            match list_get_retained(regs[base + *list as usize], regs[base + *index as usize]) {
                Some(element) => {
                    set_reg(regs, base, *dst, element);
                    noeta_jit_abi::OUTCOME_CONTINUE
                }
                None => bail,
            }
        }
        Op::LoadField {
            dst,
            obj,
            field,
            span: _,
            // The interpreter's per-site inline cache is loop-local and deliberately unused here
            // (see the note below); the miss path this helper always takes is the same read.
            cache: _,
        } => {
            // The interpreter's inline-cache lookup (`caches` is loop-local) is skipped here; the
            // cache-miss resolution — `slot_of` then `slot_at` — is the same read and is bailed on
            // exactly where the interpreter would raise (unknown field / non-object receiver). A
            // tier-1 inline cache on this path was measured (J6 investigation) and does *not* help: a
            // shape-pointer guard costs about as much as the short field-name scan it would replace,
            // and the real floor is this helper call itself — only a call-free native read (which
            // needs a layout-stable object representation) beats the interpreter. See plans/jit.
            let field = module.name(*field);
            let v = regs[base + *obj as usize];
            match v
                .shape()
                .and_then(|sh| sh.slot_of(field))
                .and_then(|s| v.slot_at(s))
            {
                Some(value) => {
                    retain(value);
                    set_reg(regs, base, *dst, value);
                    noeta_jit_abi::OUTCOME_CONTINUE
                }
                None => bail, // unknown field / non-object → interpreter raises the error
            }
        }
        Op::SetField {
            dst,
            obj,
            field,
            value,
            reuse,
            span: _,
        } => {
            let field = module.name(*field);
            if vm.set_field_fast(regs, base, *dst, *obj, field, *value, *reuse) {
                noeta_jit_abi::OUTCOME_CONTINUE
            } else {
                bail // unknown field → interpreter raises the error
            }
        }
        Op::Index {
            dst,
            recv,
            index,
            span: _,
        } => {
            let v = regs[base + *recv as usize];
            let idx = regs[base + *index as usize];
            // An `Index` trait dispatch (`o[i]` on a user object → `get`) pushes a frame — bail. Every
            // error case (out-of-bounds, wrong index type, missing key, non-indexable) also bails so the
            // interpreter raises the exact diagnostic; each of these returns before any register write.
            if v.is_object() {
                return bail;
            }
            if let Some(len) = v.list_len() {
                let Some(i) = idx.as_int().filter(|&i| i >= 0 && (i as usize) < len) else {
                    return bail;
                };
                set_reg(regs, base, *dst, list_element_retained(v, i as usize));
                noeta_jit_abi::OUTCOME_CONTINUE
            } else if v.is_map() {
                let Some(element) = idx.with_str(|key| v.map_get(key)).flatten() else {
                    return bail; // non-string key or missing key → interpreter raises
                };
                retain(element);
                set_reg(regs, base, *dst, element);
                noeta_jit_abi::OUTCOME_CONTINUE
            } else if let Some(s) = v.as_string() {
                let Some(i) = idx
                    .as_int()
                    .filter(|&i| i >= 0 && (i as usize) < s.chars().count())
                else {
                    return bail;
                };
                let ch = s.chars().nth(i as usize).unwrap().to_string();
                set_reg(regs, base, *dst, Value::string(&ch));
                noeta_jit_abi::OUTCOME_CONTINUE
            } else {
                bail // non-indexable → interpreter raises
            }
        }
        Op::MakeTuple { dst, items } => {
            // No bail path — construction never fails.
            let tuple = make_tuple(items, regs, base);
            set_reg(regs, base, *dst, tuple);
            noeta_jit_abi::OUTCOME_CONTINUE
        }
        Op::TupleIndex {
            dst,
            receiver,
            index,
            span: _,
        } => {
            // Positional projection `receiver.N`, retaining the element into `dst` — the companion to
            // the native `ListGet` for `for (i, x) in xs.enumerate()` loops. Out of range bails so the
            // interpreter raises (the checker makes this unreachable for well-typed code).
            let v = regs[base + *receiver as usize];
            match tuple_element_retained(v, *index as usize) {
                Some(element) => {
                    set_reg(regs, base, *dst, element);
                    noeta_jit_abi::OUTCOME_CONTINUE
                }
                None => bail,
            }
        }
        _ => bail,
    }
}

/// The 11 runtime-helper symbols generated tier-1 code links against, as ONE table (audit
/// finding 10): [`Vm::init_jit`] (the synchronous oracle engine) borrows it directly and
/// [`Vm::init_jit_service`] (the off-thread production service) maps each pointer to a `usize`
/// for the thread hand-off. Previously the two inits built the same 11 `(name, ptr)` pairs
/// verbatim, and a helper added to one but not the other failed only at JIT-time symbol
/// resolution — one list makes that miss impossible.
#[cfg(feature = "jit")]
fn jit_helpers() -> [(&'static str, *const u8); 11] {
    [
        (noeta_jit_abi::OBSERVE_HELPER, jit_observe as *const u8),
        (
            noeta_jit_abi::NOTE_GLOBAL_BOUND_HELPER,
            jit_note_global_bound as *const u8,
        ),
        (noeta_jit_abi::FMOD_HELPER, jit_fmod as *const u8),
        (noeta_jit_abi::RETAIN_HELPER, jit_retain as *const u8),
        (noeta_jit_abi::RELEASE_HELPER, jit_release as *const u8),
        (
            noeta_jit_abi::RELEASE_VALUE_HELPER,
            jit_release_value as *const u8,
        ),
        (noeta_jit_abi::CALL_HELPER, jit_call as *const u8),
        (noeta_jit_abi::RETURN_HELPER, jit_return as *const u8),
        (
            noeta_jit_abi::PREPARE_CALL_HELPER,
            jit_prepare_call as *const u8,
        ),
        (
            noeta_jit_abi::AFTER_CALL_HELPER,
            jit_after_call as *const u8,
        ),
        (noeta_jit_abi::LEAF_OP_HELPER, jit_run_leaf_op as *const u8),
    ]
}

impl<'m> Vm<'m> {
    /// Build the tier-1 JIT engine and, when `force_jit` is set, eagerly compile every prototype so
    /// the whole run goes through tier 1 (the oracle path). Registers the runtime-helper symbols the
    /// generated code links against. If the host ISA is unavailable the JIT stays `None` and the run
    /// interprets — behaviour is identical either way (J0 always bails to tier 0).
    #[cfg(feature = "jit")]
    pub(crate) fn init_jit(&mut self) {
        let helpers = jit_helpers();
        let template = self
            .tier1
            .jit_frame_template
            .get_or_insert_with(fresh_frame_template);
        let template_ptr = template.as_ref() as *const Frame as *const u8;
        match noeta_jit::Jit::new(&helpers, frame_layout(), template_ptr) {
            Ok(mut jit) => {
                if self.tier1.force_jit {
                    for p in 0..self.module.protos.len() {
                        if let Ok(f) = jit.compile(self.module, p) {
                            let fast = jit.get_fast(p);
                            self.jit_install(p, f, fast);
                        }
                    }
                }
                self.tier1.jit = Some(jit);
            }
            Err(_) => self.tier1.jit = None,
        }
    }

    /// Start the **off-thread** tier-1 compile service (P-PAR S4) — the production hot-counter
    /// path. Mutually exclusive with [`init_jit`](Self::init_jit) (the `force_jit` oracle's
    /// synchronous engine). Needs the module by `Arc` because the compile thread outlives every
    /// borrow the mutator holds.
    #[cfg(feature = "jit")]
    pub(crate) fn init_jit_service(&mut self, module: Arc<Module>) {
        // The shared helper table, each pointer as a `usize` so the list can cross to the
        // compile thread (a raw pointer is not `Send`; the addresses themselves are immortal).
        let helpers: Vec<(&'static str, usize)> = jit_helpers()
            .into_iter()
            .map(|(name, ptr)| (name, ptr as usize))
            .collect();
        let template = self
            .tier1
            .jit_frame_template
            .get_or_insert_with(fresh_frame_template);
        let template_addr = template.as_ref() as *const Frame as usize;
        self.tier1.jit_service =
            jit_service::JitService::spawn(module, helpers, frame_layout(), template_addr);
    }

    /// Bind a linked AOT dispatch table into the mirror tables (P-AOT L3.2b) — see
    /// [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`] for the layout (`[count][main_0, fast_0, …]`, pointer-width
    /// words). Each non-null main slot is a finalized `CompiledFn`-ABI entry point; null slots
    /// (interpreted prototype, or no fast body) are skipped.
    ///
    /// # Safety
    /// `dispatch` must point at a valid table of that layout whose entry pointers stay valid for the
    /// VM's lifetime.
    #[cfg(feature = "jit-rt")]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn bind_aot_dispatch(&mut self, dispatch: *const usize) {
        if dispatch.is_null() {
            return;
        }
        // SAFETY: word 0 is the prototype count; words then come in (main, fast) pairs (contract).
        let count = unsafe { *dispatch };
        for p in 0..count {
            let main = unsafe { *dispatch.add(1 + 2 * p) };
            let fast = unsafe { *dispatch.add(1 + 2 * p + 1) };
            if main != 0 {
                // SAFETY: a non-null slot is a finalized entry with the `CompiledFn` ABI, exactly the
                // pointer `finalize_ptr` transmutes — here it arrives as a linker-resolved address.
                let entry = unsafe {
                    std::mem::transmute::<*const u8, noeta_jit_abi::CompiledFn>(main as *const u8)
                };
                self.jit_install(p, entry, (fast != 0).then_some(fast));
            }
        }
    }

    /// Install a compiled prototype into the mirror tables — the single lookup source for the
    /// dispatch loop and the native call helpers, in both sync and service modes.
    #[cfg(feature = "jit-rt")]
    fn jit_install(&mut self, proto: usize, entry: noeta_jit_abi::CompiledFn, fast: Option<usize>) {
        if proto >= self.tier1.jit_entries.len() {
            self.tier1.jit_entries.resize(proto + 1, None);
            self.tier1.jit_fast.resize(proto + 1, None);
        }
        self.tier1.jit_entries[proto] = Some(entry);
        self.tier1.jit_fast[proto] = fast;
    }

    /// The mirrored tier-1 entry point for `proto`, if compiled.
    #[cfg(feature = "jit-rt")]
    fn jit_entry(&self, proto: usize) -> Option<noeta_jit_abi::CompiledFn> {
        self.tier1.jit_entries.get(proto).copied().flatten()
    }

    /// Whether the frame-entry loop should consult the native mirror tables. Armed when the sync
    /// engine or the off-thread service is present (JIT builds), or when entries were bound ahead of
    /// time (`aot`). Under `aot`-without-`jit` there is no compiler, so only the AOT-bound flag counts.
    #[cfg(feature = "jit-rt")]
    #[inline(always)]
    pub(crate) fn native_dispatch_armed(&self) -> bool {
        #[cfg(feature = "jit")]
        {
            self.tier1.jit.is_some() || self.tier1.jit_service.is_some() || self.tier1.aot
        }
        #[cfg(not(feature = "jit"))]
        {
            self.tier1.aot
        }
    }

    /// Drain the service mailbox into the mirror tables (service mode, only while requests are
    /// in flight). A failed compile (`entry: None`) declines its prototype — same terminal state
    /// as the worthiness gates — so every request reaches a fixed point and `jit_pending` always
    /// returns to zero.
    #[cfg(feature = "jit")]
    fn jit_drain_service(&mut self) {
        if self.tier1.jit_pending == 0 {
            return;
        }
        let Some(service) = self.tier1.jit_service.as_ref() else {
            self.tier1.jit_pending = 0;
            return;
        };
        for done in service.drain() {
            self.tier1.jit_pending = self.tier1.jit_pending.saturating_sub(1);
            match done.entry {
                Some(entry) => self.jit_install(done.proto, entry, done.fast),
                None => {
                    if done.proto >= self.tier1.jit_declined.len() {
                        self.tier1.jit_declined.resize(done.proto + 1, false);
                    }
                    self.tier1.jit_declined[done.proto] = true;
                }
            }
        }
    }

    /// Tier-0/tier-1 dispatch at a frame `'reload` (P-JIT). `entry_pc` is where native execution
    /// should resume — `0` for a fresh frame, or a post-call resume pc when re-entering a compiled
    /// frame after its callee returned (J3 resume-native). Returns what the interpreter should do next
    /// (the deopt contract). `None` when the prototype is not compiled and the interpreter should run
    /// it as usual. Hot-counter promotion happens only on a fresh entry (`entry_pc == 0`), so a resume
    /// never compiles — it only re-enters an already-native frame.
    #[cfg(feature = "jit-rt")]
    #[allow(unsafe_code)]
    pub(crate) fn jit_enter(
        &mut self,
        proto: usize,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        base: usize,
        entry_pc: usize,
    ) -> Option<JitOutcome> {
        let f = match self.jit_entry(proto) {
            Some(f) => f,
            // Only a fresh entry drives compilation; a resume at a compiled-away frame just interprets.
            // The compile trigger is compiler-only: under `aot` (no Cranelift) entries are bound ahead
            // of time, so a miss simply interprets — there is nothing to compile.
            #[cfg(feature = "jit")]
            None if entry_pc == 0 => self.jit_maybe_compile(proto)?,
            None => return None,
        };
        // Trampoline profiler seam (`noeta profile --jit`, tier-1 sampling): record the JIT frame the
        // sampler attributes the upcoming native segment's wall time to — native code hits no op
        // boundary, so `before_op` can't fire while it runs. `None` on every non-profiled run — one
        // predicted branch. `self.module` (a `Copy` `&'m Module`) and `self.profiler` are disjoint
        // fields, borrowed independently of the `frames`/`regs` params.
        let strand = self.sched.current_strand;
        if let Some(prof) = self.profiler.as_mut() {
            let view = DebugView {
                module: self.module,
                frames: &frames[..],
                regs: &regs[..],
                globals: &self.persist.globals,
                strand,
            };
            prof.on_jit_enter(&view, proto as u32);
        }
        let vm_ptr = self as *mut Vm as *mut core::ffi::c_void;
        let regs_ptr = regs.as_mut_ptr();
        let globals_ptr = self.persist.globals.as_mut_ptr();
        let frames_ptr = frames as *mut Vec<Frame> as *mut core::ffi::c_void;
        let regs_vec_ptr = regs as *mut Vec<Value> as *mut core::ffi::c_void;
        // SAFETY: `f` is a finalized tier-1 entry point with the `CompiledFn` ABI. `regs_ptr` is the
        // frame data base (native adds `base * 8`); it is used only *before* any call, and a native
        // `Call` returns immediately (`CALLED`) without touching it again, so a `reserve_window`
        // realloc inside `jit_call` can't leave it dangling in use. `frames_ptr`/`regs_vec_ptr` let
        // `jit_call` push the callee frame and grow the shared stacks; `globals` never reallocates.
        // All pointers are live for the synchronous call.
        let raw = unsafe {
            f(
                vm_ptr,
                regs_ptr,
                base,
                globals_ptr,
                frames_ptr,
                regs_vec_ptr,
                entry_pc,
            )
        };
        // Trampoline exit: bank the native segment's wall time onto the frame recorded above.
        if let Some(prof) = self.profiler.as_mut() {
            let view = DebugView {
                module: self.module,
                frames: &frames[..],
                regs: &regs[..],
                globals: &self.persist.globals,
                strand,
            };
            prof.on_jit_exit(&view);
        }
        Some(match raw {
            noeta_jit_abi::OUTCOME_CALLED => JitOutcome::Called,
            noeta_jit_abi::OUTCOME_ABORTED => JitOutcome::Abort,
            noeta_jit_abi::OUTCOME_RETURNED => JitOutcome::Returned,
            noeta_jit_abi::OUTCOME_HALTED => JitOutcome::Halted,
            pc => JitOutcome::Bail(pc as usize),
        })
    }

    /// Bump prototype `proto`'s entry counter and, once it is hot (or immediately under `force_jit`),
    /// promote it. Synchronous mode compiles in place and returns the fresh entry point on the
    /// promoting call; **service mode** (P-PAR S4) queues the compile off-thread and keeps
    /// interpreting — the entry lands in the mirror via the mailbox drain a later call performs.
    /// `None` while still cold, queued, or when the JIT is unavailable.
    #[cfg(feature = "jit")]
    fn jit_maybe_compile(&mut self, proto: usize) -> Option<noeta_jit_abi::CompiledFn> {
        if self.tier1.jit.is_none() && self.tier1.jit_service.is_none() {
            return None;
        }
        // Harvest any compiles that landed since the last checkpoint (no-op at zero pending),
        // then re-check the mirror — the promoting entry may already be ready.
        self.jit_drain_service();
        if let Some(f) = self.jit_entry(proto) {
            return Some(f);
        }
        // Already found not worth compiling (a prototype whose only loops bail) → keep interpreting.
        if self.tier1.jit_declined.get(proto).copied().unwrap_or(false) {
            return None;
        }
        if proto >= self.tier1.jit_counters.len() {
            self.tier1.jit_counters.resize(proto + 1, 0);
        }
        self.tier1.jit_counters[proto] = self.tier1.jit_counters[proto].saturating_add(1);
        let hot = self.tier1.force_jit || self.tier1.jit_counters[proto] >= JIT_HOT_THRESHOLD;
        if !hot {
            return None;
        }
        // A prototype dominated by a bailing loop bounces tier-0↔tier-1 every iteration, slower than
        // the interpreter — decline it once (the oracle's `force_jit` compiles everything anyway).
        if !self.tier1.force_jit && !noeta_jit::worth_compiling(&self.module.protos[proto]) {
            if proto >= self.tier1.jit_declined.len() {
                self.tier1.jit_declined.resize(proto + 1, false);
            }
            self.tier1.jit_declined[proto] = true;
            return None;
        }
        if self.tier1.jit_service.is_some() {
            self.jit_request(proto, false);
            return None;
        }
        let module = self.module;
        let jit = self.tier1.jit.as_mut()?;
        let f = jit.compile(module, proto).ok()?;
        let fast = jit.get_fast(proto);
        self.jit_install(proto, f, fast);
        Some(f)
    }

    /// Queue `proto` for off-thread compilation, exactly once (service mode). `osr` marks a
    /// request born at a loop back-edge, so the landing entry OSR-enters mid-loop.
    #[cfg(feature = "jit")]
    fn jit_request(&mut self, proto: usize, osr: bool) {
        if self
            .tier1
            .jit_requested
            .get(proto)
            .copied()
            .unwrap_or(false)
        {
            return;
        }
        if proto >= self.tier1.jit_requested.len() {
            self.tier1.jit_requested.resize(proto + 1, false);
        }
        self.tier1.jit_requested[proto] = true;
        if osr {
            if proto >= self.tier1.jit_osr_pending.len() {
                self.tier1.jit_osr_pending.resize(proto + 1, false);
            }
            self.tier1.jit_osr_pending[proto] = true;
        }
        let sent = self
            .tier1
            .jit_service
            .as_ref()
            .is_some_and(|service| service.request(proto));
        if sent {
            self.tier1.jit_pending += 1;
        } else {
            // The service thread is gone: decline so no caller waits on a response forever.
            if proto >= self.tier1.jit_declined.len() {
                self.tier1.jit_declined.resize(proto + 1, false);
            }
            self.tier1.jit_declined[proto] = true;
        }
    }

    /// On-stack replacement trigger (P-JIT J5): a taken **backward branch** in prototype `proto` is a
    /// loop back-edge. Count it toward the hot threshold and, once the prototype crosses it, compile
    /// the prototype — returning `true` to signal the inner loop to re-enter native code at the loop
    /// header (the compiled body has an OSR entry block for every loop header). `false` = keep
    /// interpreting.
    ///
    /// This closes the hole where a long-running loop never gets hot: promotion otherwise counts only
    /// frame *entries*, so a top-level program that is one big loop (its `main` frame entered exactly
    /// once) would run entirely in tier 0. Counting back-edges makes such a loop promote and jump into
    /// native code mid-flight.
    ///
    /// **One OSR per prototype.** If the prototype is already compiled we do nothing: the frame goes
    /// native at its next `'reload` anyway, and re-OSRing from tier 0 (after a native op bailed back)
    /// would risk bouncing tier-0↔tier-1 every iteration for a loop whose body native can't sustain.
    #[cfg(feature = "jit")]
    pub(crate) fn jit_osr_backedge(&mut self, proto: usize) -> bool {
        if self.jit_entry(proto).is_some() {
            // Service mode: a back-edge-born compile just landed in the mirror — take the one
            // pending OSR entry now (a single long-running loop gets no other chance to go
            // native mid-flight). A prototype compiled via the call-entry path has no pending
            // OSR and keeps the one-OSR-per-prototype rule: it goes native at its next `'reload`.
            if self
                .tier1
                .jit_osr_pending
                .get(proto)
                .copied()
                .unwrap_or(false)
            {
                self.tier1.jit_osr_pending[proto] = false;
                return true;
            }
            return false;
        }
        // Already found un-sustainable (all loops bail) → keep interpreting, no per-iteration re-scan.
        if self.tier1.jit_declined.get(proto).copied().unwrap_or(false) {
            return false;
        }
        // A back-edge-born request is in flight: harvest the mailbox; enter the moment it lands.
        if self
            .tier1
            .jit_requested
            .get(proto)
            .copied()
            .unwrap_or(false)
        {
            self.jit_drain_service();
            if self.jit_entry(proto).is_some()
                && self
                    .tier1
                    .jit_osr_pending
                    .get(proto)
                    .copied()
                    .unwrap_or(false)
            {
                self.tier1.jit_osr_pending[proto] = false;
                return true;
            }
            return false;
        }
        // Bump the back-edge counter; only decide once the prototype is hot. `force_jit` (the oracle)
        // compiles everything for full coverage, so it skips the worthiness gate.
        if proto >= self.tier1.jit_counters.len() {
            self.tier1.jit_counters.resize(proto + 1, 0);
        }
        self.tier1.jit_counters[proto] = self.tier1.jit_counters[proto].saturating_add(1);
        if !(self.tier1.force_jit || self.tier1.jit_counters[proto] >= JIT_HOT_THRESHOLD) {
            return false;
        }
        if !self.tier1.force_jit && !noeta_jit::worth_osr(&self.module.protos[proto]) {
            // A heap-op-dominated loop: native would bounce tier-0↔tier-1 every iteration, slower than
            // the interpreter. Decline OSR for this prototype, once and for good.
            if proto >= self.tier1.jit_declined.len() {
                self.tier1.jit_declined.resize(proto + 1, false);
            }
            self.tier1.jit_declined[proto] = true;
            return false;
        }
        if self.tier1.jit_service.is_some() {
            self.jit_request(proto, true);
            return false;
        }
        let module = self.module;
        let jit = match self.tier1.jit.as_mut() {
            Some(j) => j,
            None => return false,
        };
        match jit.compile(module, proto) {
            Ok(f) => {
                let fast = jit.get_fast(proto);
                self.jit_install(proto, f, fast);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    /// The `..` ban in this file's op patterns, checked on the source rather than trusted.
    ///
    /// The tier-1 helpers each read an `Op` back out of the module and act on its fields. Two of
    /// them — `jit_prepare_call`'s `Op::Call` and `Op::CallGlobal` arms — copy a call's arguments
    /// into the callee window **positionally**. When those arms wrote `..`, adding the
    /// `type_args` channel to the call ops did not break them: they compiled, ran, and silently
    /// dropped the type arguments, laying the first *value* argument into the callee's `$ty0`
    /// slot, where it would be read as an index into the program's type table. A wrong answer,
    /// not a crash — the failure mode a compile error would have caught for free.
    ///
    /// `jit_call`, three lines away, already named every field, so the same change *did* break it.
    /// That asymmetry is the whole point: a field-exhaustive pattern turns "a new operand needs
    /// handling here" into a compile error, and a `..` gives that up for every field that will
    /// ever be added. Deliberately-unused fields bind as `field: _` and say so.
    #[test]
    fn the_tier1_helpers_bind_every_field() {
        let src = include_str!("tier1.rs");
        // The helpers end where this test module begins; the doc comment above legitimately talks
        // about `..` and is not a pattern.
        let helpers = src.split("mod tests {").next().expect("a source body");
        let offenders: Vec<&str> = helpers
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.starts_with("//")
                    && (t.ends_with("..")
                        || t.contains(".. }")
                        || t.contains(".. =>")
                        || t == "..,")
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "`..` is banned in this file's op patterns — bind every field, deliberately-unused \
             ones as `field: _`, so that adding an operand to a bytecode op is a compile error \
             here rather than a silently dropped operand:\n  {}",
            offenders.join("\n  ")
        );
    }
}
