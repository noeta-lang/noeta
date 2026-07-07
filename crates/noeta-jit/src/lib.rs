//! The `lang` method JIT (milestone P-JIT) — a Cranelift backend that native-compiles hot
//! prototypes so the fast path runs as machine code instead of dispatched register bytecode.
//!
//! # Where this sits
//!
//! The interpreter ([`noeta_vm`](../noeta_vm/index.html)) is **tier 0**: every prototype runs by
//! `match`-dispatching its [`noeta_bytecode::Op`]s. This crate is **tier 1**: a per-prototype
//! [`CompiledFn`] the VM calls *instead of* entering the inner dispatch loop for that frame. A
//! compiled function operates directly on the VM's shared contiguous register stack (P-VMT-FRAME) at
//! `regs[base + i]`, and returns the **bytecode `pc` at which the interpreter should resume**.
//!
//! # The tier-0/tier-1 handoff (the deopt contract)
//!
//! A [`CompiledFn`] never runs a whole frame to completion. It runs the ops it knows how to compile
//! natively and, at the first op it does *not* (a `Return`, a `Halt`, or a guard that fails), it
//! **returns the `pc` of that op** and lets the interpreter take over there. This works because the
//! native code performs the exact same register writes as the interpreter would — so when it hands
//! back at `pc`, the register window is already in the state the interpreter expects at `pc`. The
//! shared register stack (P-VMT-FRAME) makes the handoff free: there is nothing to reconstruct.
//!
//! # J1 — the integer fast path
//!
//! J1 compiles prototypes whose every op is in the integer subset: `LoadConst` (of an immediate
//! constant), `Move`, `Drop`, integer `Binary` (`+ - * / %` and `== != < <= > >=`), `Jump`,
//! `JumpIfTrue`/`JumpIfFalse`, and `CondBranch`; `Return`/`Halt` are bail points. Everything else
//! makes the prototype ineligible (it gets a bail stub → interpreted). The generated code:
//!
//! - **Guards the parameters at entry**: if any argument is a heap pointer it bails to `pc 0` (the
//!   interpreter runs the frame). Locals start as `unit`, so once the params are proven immediate,
//!   *every register holds an immediate for the whole native run* — which is why the fast path needs
//!   **zero** refcount operations (every `retain`/`release` the interpreter would do is a no-op on an
//!   immediate). This is the invariant the whole slice rests on.
//! - **Guards each integer op**: a `Binary` bails if an operand is not a small int, if a `/`/`%`
//!   divisor is zero, or if a result overflows the 48-bit immediate range (so a would-be heap-boxed
//!   big int is produced by the interpreter, not here). A `CondBranch` bails on a non-bool condition
//!   (to reproduce E0007). Bailing always happens *before* any register write, so the interpreter
//!   re-executes the op from clean state.
//!
//! The value encoding is [`noeta_value::Value::NANBOX`] — the single source of truth, so the inlined
//! tag checks and box/unbox sequences match the interpreter bit-for-bit.
//!
//! # Gating
//!
//! The whole JIT lives behind the `jit` cargo feature on `noeta-vm`/`noeta-conformance`. The default
//! build, the deterministic sandbox, and the conformance differential never pull Cranelift and are
//! byte-identical without it — the same discipline that gates the real-thread isolates.

mod plan;

use core::ffi::c_void;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Value as ClValue, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module as _, default_libcall_names};

use noeta_ast::BinaryOp;
use noeta_bytecode::{Const, Module, Op, Reg};
use noeta_value::Value;

/// The in-memory layout of the VM's call frame and its frame/register stacks — the ABI contract
/// between `noeta-vm`'s `Frame`/`Vec<Frame>`/`Vec<Value>` and the JIT's native call-frame codegen
/// (P-CALL: inline the reserve-window / push-Frame / pop sequence with no per-call helper). Every
/// field is a byte offset or size; `noeta-vm` fills them from `offset_of!`/`size_of!` on its own
/// `Frame` and a one-time `Vec`-header probe, then hands this to the JIT, which bakes the numbers
/// into code generated **in the same process** — so a layout change can never desync (a `noeta-vm`
/// lock test asserts every offset locates the real field). This is the frame analogue of
/// [`noeta_value::Value::NANBOX`]: the single source of truth so native codegen can't drift.
#[derive(Debug, Clone, Copy)]
pub struct FrameLayout {
    /// `size_of::<Frame>()` — the stride between frames in the `Vec<Frame>` buffer.
    pub frame_size: usize,
    /// `align_of::<Frame>()`.
    pub frame_align: usize,
    /// Byte offset of each `Frame` scalar field the native fast path writes.
    pub proto_offset: usize,
    pub base_offset: usize,
    pub pc_offset: usize,
    pub ret_dst_offset: usize,
    /// Byte offset of the two fields the native fast path initializes to their empty values
    /// (`ret_transform = None`, `upvalues = Vec::new()`) — a plain top-level `fn` frame carries
    /// neither a return transform nor upvalues.
    pub ret_transform_offset: usize,
    pub upvalues_offset: usize,
    /// A `Vec<T>` is three pointer-sized words; these are the *word* indices (0, 1, or 2) of its
    /// data pointer, length, and capacity. The header layout is `T`-independent (only the element
    /// stride differs), so one probe serves both the `Vec<Frame>` and `Vec<Value>` stacks.
    pub vec_ptr_word: usize,
    pub vec_len_word: usize,
    pub vec_cap_word: usize,
}

/// A compiled prototype's entry point — the tier-1 ABI.
///
/// - `vm` is an opaque `*mut Vm` (the interpreter reconstitutes `&mut Vm` from it to service
///   runtime-helper callbacks); this crate never dereferences it.
/// - `regs` points at the base of the VM's shared register stack (`Vec<Value>`), and `base` is the
///   frame's window offset, so the frame's registers are `regs[base + i]` — identical addressing to
///   the interpreter (P-VMT-FRAME).
/// - `globals` points at the base of the VM's global-slot array (`Vec<Value>`, one word per
///   [`noeta_bytecode::GlobalId`]); it never grows, so the pointer is stable for the whole run. Native
///   `LoadGlobal`/`StoreGlobal`/`TakeGlobal` index it directly.
/// - `frames` and `regs_vec` are opaque handles to the interpreter's `Vec<Frame>` and `Vec<Value>`
///   (the frame and register stacks), passed to the `jit_call` helper so a native call can push the
///   callee frame and grow the shared stack — the contiguous-stack call path (J3). This crate never
///   dereferences them.
/// - `entry_pc` is the bytecode pc at which native execution resumes: `0` for a fresh frame (the
///   parameter guard runs), or a post-call resume pc when the interpreter re-enters a compiled frame
///   after its callee returned (J3 resume-native). An `entry_pc` the compiled code has no entry for
///   is returned as a bail (the interpreter keeps running that frame).
///
/// The `i64` return encodes the outcome: a non-negative value is a **resume pc** (the frame bailed
/// there); [`OUTCOME_CALLED`] means a callee frame was pushed (the interpreter should run it);
/// [`OUTCOME_ABORTED`] means the frame aborted (a diagnostic is on the VM).
///
/// `extern "C"` so Cranelift's platform calling convention matches the pointer this is transmuted
/// from.
pub type CompiledFn = unsafe extern "C" fn(
    vm: *mut c_void,
    regs: *mut Value,
    base: usize,
    globals: *mut Value,
    frames: *mut c_void,
    regs_vec: *mut c_void,
    entry_pc: usize,
) -> i64;

/// [`CompiledFn`] return sentinel: a callee frame was pushed onto the frame stack (a native `Call`);
/// the interpreter should re-derive the top frame and run it.
pub const OUTCOME_CALLED: i64 = -1;
/// [`CompiledFn`] return sentinel: the frame aborted (a diagnostic is recorded on the VM); the
/// interpreter should propagate the unwind.
pub const OUTCOME_ABORTED: i64 = -2;
/// [`CompiledFn`] return sentinel: the frame ran its `Return` — it transferred the result into its
/// caller's destination register and popped itself, so the caller is now the top frame. The
/// interpreter re-derives that caller (`continue 'reload`); a native direct caller resumes with the
/// result already in place.
pub const OUTCOME_RETURNED: i64 = -3;
/// [`CompiledFn`] return sentinel: the **bottom** frame returned (there was no caller). The run is
/// over; its value is on the VM (`jit_ret`). Only reached via the interpreter seam, never a direct
/// call.
pub const OUTCOME_HALTED: i64 = -4;
/// Internal outcome (never returned by a [`CompiledFn`]): the `jit_after_direct_call` helper's signal
/// that a native direct call's callee returned cleanly, so the native caller continues in place.
pub const OUTCOME_CONTINUE: i64 = -5;

/// The name the bail stub calls to prove the runtime-helper ABI links. The VM registers a pointer
/// for this symbol when it constructs the [`Jit`].
pub const OBSERVE_HELPER: &str = "noeta_jit_observe";

/// The name of the "note a global's first binding" helper. Native `StoreGlobal` writes the slot
/// itself, then calls this so the VM records the slot in `global_order` for reverse-order teardown
/// destruction — the one piece of `StoreGlobal` that can't be inlined (a `Vec` push may reallocate).
pub const NOTE_GLOBAL_BOUND_HELPER: &str = "noeta_jit_note_global_bound";

/// Runtime-helper names for the heap/refcount path a call-bearing prototype needs (J3). `retain`
/// bumps a value's refcount; `release` drops one (matching the interpreter's `set_reg` overwrite);
/// `release_value` is the destructor-aware drop (for `Drop`-relevant); `call` runs the shared
/// `Op::Call` setup on the interpreter's stacks.
pub const RETAIN_HELPER: &str = "noeta_jit_retain";
pub const RELEASE_HELPER: &str = "noeta_jit_release";
pub const RELEASE_VALUE_HELPER: &str = "noeta_jit_release_value";
pub const CALL_HELPER: &str = "noeta_jit_call";
/// The `Op::Return` helper (J3 native calls): runs the shared return protocol (transfer to the
/// caller, pop the frame) and returns [`OUTCOME_RETURNED`] or [`OUTCOME_HALTED`].
pub const RETURN_HELPER: &str = "noeta_jit_return";
/// Direct-call helpers (J3 native→native calls). `prepare_call` checks whether the `Op::Call` at a pc
/// can be a direct native call (compiled callee, plain arity, no upvalues, stack capacity) and, if so,
/// sets up the callee frame and returns two words — the callee's compiled entry pointer (else `0`, a
/// fallback to `call`) and its reserved window base (P-JSSA S4.0: one roundtrip, not two);
/// `after_call` inspects the callee's outcome and tells the caller to continue in place
/// ([`OUTCOME_CONTINUE`]) or propagate.
pub const PREPARE_CALL_HELPER: &str = "noeta_jit_prepare_call";
pub const AFTER_CALL_HELPER: &str = "noeta_jit_after_call";
/// The leaf-heap-op helper (J4): runs a single non-dispatching heap/collection op (the interpreter's
/// exact arm, refcounts included) and returns [`OUTCOME_CONTINUE`] (done — the caller advances) or a
/// resume pc (it can't handle this instance — a dispatch or an error — so the interpreter runs it).
pub const LEAF_OP_HELPER: &str = "noeta_jit_run_leaf_op";

/// A per-call-site **inline cache** slot (P-JSSA S4.2), allocated by the JIT (one per
/// `Call`/`CallGlobal` pc, stable address baked into the code) and filled by the VM's
/// `jit_prepare_call` when it resolves a fast-convention callee. Layout:
/// `[key, untagged fast entry, callee num_registers, callee proto]`.
///
/// `key` is the callee closure's exact NaN-box bits. The VM **pins** (retains + roots until
/// teardown) every closure it caches, so bits-equality on a later call proves it is the same
/// live object — the same prototype — with no ABA hazard (only 0-upvalue closures are cacheable,
/// and those hold nothing, so delaying their free to teardown is observably inert). A site that
/// ever sees a *second* distinct callee is poisoned (never cached again), bounding pins by site
/// count. The two sentinels are bit patterns no live value can have.
pub type CallSiteCache = [u64; 4];

/// [`CallSiteCache`] key sentinel: never filled. The unbound-global marker can appear in a
/// global *slot* but never as a call operand (`prepare_call` rejects it first).
pub const SITE_EMPTY: u64 = {
    let l = Value::NANBOX;
    l.unbound_bits
};

/// [`CallSiteCache`] key sentinel: poisoned (a second distinct callee was seen). The pattern is
/// a NaN-boxed heap pointer with address 0 — the heap never allocates at null.
pub const SITE_POISON: u64 = {
    let l = Value::NANBOX;
    l.sign_bit | l.qnan
};

/// The method JIT: a Cranelift [`JITModule`] plus a per-prototype cache of finalized entry points.
///
/// The cache is indexed by prototype index (into [`noeta_bytecode::Module::protos`]) — the same key
/// the interpreter dispatches on. `compiled[p]` is `Some` once prototype `p` has been JIT-compiled;
/// the interpreter consults it at frame entry.
pub struct Jit {
    /// Owns every finalized machine-code page; must outlive every [`CompiledFn`] handed out.
    module: JITModule,
    /// Finalized entry points, keyed by prototype index. `None` = not (yet) compiled → tier 0.
    compiled: Vec<Option<CompiledFn>>,
    /// Fast-convention entry points (P-JSSA S4.1), keyed by prototype index: the type-erased
    /// pointer to the prototype's second, frameless-window body — signature
    /// `(vm, regs, base, globals, frames, regs_vec, arg0..argN) -> (outcome, value)`, one `i64`
    /// argument per parameter. `None` = the prototype has no fast body (ineligible or not yet
    /// compiled); calls then use the classic direct path.
    fast_compiled: Vec<Option<usize>>,
    /// The VM's frame/`Vec`-header layout, baked into the fast bodies' native frame pop
    /// (see [`FrameLayout`]).
    layout: FrameLayout,
    /// A prototype "empty" `Frame` owned by the VM (`Jit::new`'s `frame_template`): `proto`/
    /// `base`/`ret_dst` zeroed, `pc = 0`, `ret_transform = None`, `upvalues` empty. The S4.2
    /// native frame push copies its `frame_size` bytes and patches the three per-call fields —
    /// no enum-discriminant or `Vec`-internals knowledge in the JIT.
    frame_template: *const u8,
    /// The inline-cache slots (S4.2), one per emitted call site. Each is individually boxed —
    /// not inline in the `Vec` — because the generated code bakes the slot's address: a `Vec`
    /// reallocation would move inline slots and dangle every baked pointer.
    #[allow(clippy::vec_box)]
    site_slots: Vec<Box<CallSiteCache>>,
    /// How many prototypes were compiled to *real* native code (vs a bail stub) — the coverage stat.
    native_count: usize,
    /// Total / worst-case wall time spent inside [`Jit::compile`] (P-PAR S0c): every compile runs
    /// synchronously on the mutator thread today, so these are the pauses the program felt. Cache
    /// hits don't count — only actual codegen work.
    compile_ns_total: u64,
    compile_ns_max: u64,
    /// Imported runtime helpers, declared once (see the `*_HELPER` name constants).
    observe_id: FuncId,
    note_bound_id: FuncId,
    retain_id: FuncId,
    release_id: FuncId,
    release_value_id: FuncId,
    call_id: FuncId,
    return_id: FuncId,
    prepare_call_id: FuncId,
    after_call_id: FuncId,
    leaf_op_id: FuncId,
    ctx: cranelift_codegen::Context,
    fb_ctx: FunctionBuilderContext,
}

impl std::fmt::Debug for Jit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jit")
            .field(
                "compiled",
                &self.compiled.iter().filter(|c| c.is_some()).count(),
            )
            .field("native", &self.native_count)
            .field("protos", &self.compiled.len())
            .finish()
    }
}

impl Jit {
    /// Build a JIT engine, registering the runtime-helper symbols the generated code may call.
    /// Each `(name, ptr)` is a `*const u8` cast of an `extern "C"` Rust function the VM owns;
    /// Cranelift resolves calls to `name` against `ptr`. The VM passes at least [`OBSERVE_HELPER`].
    ///
    /// Returns `Err` with a human-readable message if the host ISA is unavailable or Cranelift
    /// rejects the flags — the VM treats that as "JIT unavailable, stay tier 0".
    pub fn new(
        helpers: &[(&str, *const u8)],
        layout: FrameLayout,
        frame_template: *const u8,
    ) -> Result<Jit, String> {
        if !layout.frame_size.is_multiple_of(8) {
            return Err("Frame size must be word-aligned for the native frame push".to_string());
        }
        let mut flags = settings::builder();
        flags
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flags.set("is_pic", "false").map_err(|e| e.to_string())?;
        // P-JSSA: the SSA promotion leans on Cranelift's mid-end (block-param coalescing, GVN,
        // dead-load removal of unused entry inits). The default `opt_level=none` was fine for the
        // memory-form codegen; with block params it is not.
        //
        // `NOETA_JIT_OPT` is a **dev measurement knob** (P-PAR S4): it lets the compile-time /
        // code-quality trade be A/B'd without a rebuild (`none` compiles far faster, `speed` runs
        // faster). Semantics are identical at every level, so the jit-differential is unaffected;
        // the shipped default stays `speed`.
        let opt_level = std::env::var("NOETA_JIT_OPT").unwrap_or_else(|_| "speed".to_string());
        flags
            .set("opt_level", &opt_level)
            .map_err(|e| e.to_string())?;
        let isa_builder = cranelift_native::builder().map_err(|m| m.to_string())?;
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())?;
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        for (name, ptr) in helpers {
            builder.symbol(*name, *ptr);
        }
        let mut module = JITModule::new(builder);
        let ptr_ty = module.target_config().pointer_type();
        // `noeta_jit_observe(vm: ptr)` and `noeta_jit_note_global_bound(vm: ptr, g: i32)`, declared once.
        let mut observe_sig = module.make_signature();
        observe_sig.params.push(AbiParam::new(ptr_ty));
        let observe_id = module
            .declare_function(OBSERVE_HELPER, Linkage::Import, &observe_sig)
            .map_err(|e| e.to_string())?;
        let mut note_sig = module.make_signature();
        note_sig.params.push(AbiParam::new(ptr_ty));
        note_sig.params.push(AbiParam::new(types::I32));
        let note_bound_id = module
            .declare_function(NOTE_GLOBAL_BOUND_HELPER, Linkage::Import, &note_sig)
            .map_err(|e| e.to_string())?;
        // Heap/call helpers (J3). `retain(v: i64)`, `release(v: i64)`, `release_value(vm: ptr, v: i64)`,
        // and `call(vm, frames, regs_vec: ptr, base: usize, proto: i32, pc: i32) -> i64`.
        let mut retain_sig = module.make_signature();
        retain_sig.params.push(AbiParam::new(types::I64));
        let retain_id = module
            .declare_function(RETAIN_HELPER, Linkage::Import, &retain_sig)
            .map_err(|e| e.to_string())?;
        let release_id = module
            .declare_function(RELEASE_HELPER, Linkage::Import, &retain_sig)
            .map_err(|e| e.to_string())?;
        let mut release_value_sig = module.make_signature();
        release_value_sig.params.push(AbiParam::new(ptr_ty));
        release_value_sig.params.push(AbiParam::new(types::I64));
        let release_value_id = module
            .declare_function(RELEASE_VALUE_HELPER, Linkage::Import, &release_value_sig)
            .map_err(|e| e.to_string())?;
        let mut call_sig = module.make_signature();
        call_sig.params.push(AbiParam::new(ptr_ty)); // vm
        call_sig.params.push(AbiParam::new(ptr_ty)); // frames
        call_sig.params.push(AbiParam::new(ptr_ty)); // regs_vec
        call_sig.params.push(AbiParam::new(ptr_ty)); // base
        call_sig.params.push(AbiParam::new(types::I32)); // proto
        call_sig.params.push(AbiParam::new(types::I32)); // pc
        call_sig.returns.push(AbiParam::new(types::I64));
        let call_id = module
            .declare_function(CALL_HELPER, Linkage::Import, &call_sig)
            .map_err(|e| e.to_string())?;
        // `return(vm, frames, regs_vec: ptr, raw: i64, release_mask: i64) -> i64`. The mask is the
        // S4.0 fast teardown: which window slots may hold heap values at this return site.
        let mut return_sig = module.make_signature();
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(types::I64));
        return_sig.params.push(AbiParam::new(types::I64));
        return_sig.returns.push(AbiParam::new(types::I64));
        let return_id = module
            .declare_function(RETURN_HELPER, Linkage::Import, &return_sig)
            .map_err(|e| e.to_string())?;
        // Direct-call helpers. `prepare_call` takes `call`'s params but returns two words —
        // (fnptr-or-0, callee window base), the VM's `#[repr(C)] PreparedCall` (rax:rdx under
        // SysV, exactly what a two-i64-return Cranelift import reads back);
        // `after_call(vm, frames, outcome: i64) -> i64`.
        let mut prepare_sig = module.make_signature();
        prepare_sig.params.clone_from(&call_sig.params);
        prepare_sig.params.push(AbiParam::new(ptr_ty)); // site cache slot (S4.2), or null
        prepare_sig.returns.push(AbiParam::new(types::I64));
        prepare_sig.returns.push(AbiParam::new(types::I64));
        let prepare_call_id = module
            .declare_function(PREPARE_CALL_HELPER, Linkage::Import, &prepare_sig)
            .map_err(|e| e.to_string())?;
        let mut after_call_sig = module.make_signature();
        after_call_sig.params.push(AbiParam::new(ptr_ty)); // vm
        after_call_sig.params.push(AbiParam::new(ptr_ty)); // frames
        after_call_sig.params.push(AbiParam::new(types::I64)); // callee outcome
        after_call_sig.returns.push(AbiParam::new(types::I64));
        let after_call_id = module
            .declare_function(AFTER_CALL_HELPER, Linkage::Import, &after_call_sig)
            .map_err(|e| e.to_string())?;
        // `run_leaf_op(vm, regs_vec: ptr, base: usize, proto: i32, pc: i32) -> i64`.
        let mut leaf_sig = module.make_signature();
        leaf_sig.params.push(AbiParam::new(ptr_ty));
        leaf_sig.params.push(AbiParam::new(ptr_ty));
        leaf_sig.params.push(AbiParam::new(ptr_ty));
        leaf_sig.params.push(AbiParam::new(types::I32));
        leaf_sig.params.push(AbiParam::new(types::I32));
        leaf_sig.returns.push(AbiParam::new(types::I64));
        let leaf_op_id = module
            .declare_function(LEAF_OP_HELPER, Linkage::Import, &leaf_sig)
            .map_err(|e| e.to_string())?;
        let ctx = module.make_context();
        Ok(Jit {
            module,
            compiled: Vec::new(),
            fast_compiled: Vec::new(),
            layout,
            frame_template,
            site_slots: Vec::new(),
            native_count: 0,
            compile_ns_total: 0,
            compile_ns_max: 0,
            observe_id,
            note_bound_id,
            retain_id,
            release_id,
            release_value_id,
            call_id,
            return_id,
            prepare_call_id,
            after_call_id,
            leaf_op_id,
            ctx,
            fb_ctx: FunctionBuilderContext::new(),
        })
    }

    /// The finalized entry point for prototype `proto`, or `None` if it is not compiled (tier 0).
    pub fn get(&self, proto: usize) -> Option<CompiledFn> {
        self.compiled.get(proto).copied().flatten()
    }

    /// The fast-convention entry point for prototype `proto` (P-JSSA S4.1), or `None` if the
    /// prototype has no fast body. Type-erased: the pointer's signature depends on the
    /// prototype's arity, and only compiled callers (which know the arity statically) invoke it.
    pub fn get_fast(&self, proto: usize) -> Option<usize> {
        self.fast_compiled.get(proto).copied().flatten()
    }

    /// How many prototypes have any compiled entry (native or bail stub).
    pub fn compiled_count(&self) -> usize {
        self.compiled.iter().filter(|c| c.is_some()).count()
    }

    /// How many prototypes were compiled to *real native code* (J1-eligible) — the coverage number
    /// the oracle reports.
    pub fn native_count(&self) -> usize {
        self.native_count
    }

    /// Compile prototype `proto` of `module` and cache its entry point, returning it. A J1-eligible
    /// prototype gets a native integer body; anything else gets a bail stub (→ interpreted).
    /// Idempotent: a second call for an already-compiled prototype returns the cached entry point.
    pub fn compile(&mut self, module: &Module, proto: usize) -> Result<CompiledFn, String> {
        if proto >= self.compiled.len() {
            let n = module.protos.len().max(proto + 1);
            self.compiled.resize(n, None);
            self.fast_compiled.resize(n, None);
        }
        if let Some(f) = self.compiled[proto] {
            return Ok(f);
        }
        let compile_start = std::time::Instant::now();
        let chunk = &module.protos[proto];
        let f = if is_eligible(chunk) {
            let f = self.emit_int_body(module, proto, false)?;
            self.native_count += 1;
            // S4.1: also compile the fast-convention body where the prototype supports the
            // frameless-window contract; direct calls to it then skip the window fill, the
            // argument copy, and the helper-side return protocol.
            if fast_ok(chunk) {
                let ff = self.emit_int_body(module, proto, true)?;
                self.fast_compiled[proto] = Some(ff as usize);
            }
            f
        } else {
            self.emit_bail_stub(proto)?
        };
        self.compiled[proto] = Some(f);
        let ns = compile_start.elapsed().as_nanos() as u64;
        self.compile_ns_total += ns;
        self.compile_ns_max = self.compile_ns_max.max(ns);
        Ok(f)
    }

    /// Total wall time spent compiling across the run, in nanoseconds (P-PAR S0c).
    pub fn compile_ns_total(&self) -> u64 {
        self.compile_ns_total
    }

    /// The single longest compile — the worst pause the mutator felt, in nanoseconds (P-PAR S0c).
    pub fn compile_ns_max(&self) -> u64 {
        self.compile_ns_max
    }

    /// The tier-1 ABI signature: `(vm, regs, base, globals, frames, regs_vec) -> i64` (see
    /// [`CompiledFn`]).
    fn abi_signature(&self) -> cranelift_codegen::ir::Signature {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(ptr_ty)); // vm
        sig.params.push(AbiParam::new(ptr_ty)); // regs (frame data base)
        sig.params.push(AbiParam::new(ptr_ty)); // base (usize == pointer width)
        sig.params.push(AbiParam::new(ptr_ty)); // globals
        sig.params.push(AbiParam::new(ptr_ty)); // frames (opaque *mut Vec<Frame>)
        sig.params.push(AbiParam::new(ptr_ty)); // regs_vec (opaque *mut Vec<Value>)
        sig.params.push(AbiParam::new(ptr_ty)); // entry_pc (usize — where to resume native execution)
        sig.returns.push(AbiParam::new(types::I64)); // outcome (resume pc / CALLED / ABORTED)
        sig
    }

    /// The fast-convention signature for an `arity`-parameter prototype (P-JSSA S4.1):
    /// `(vm, regs, base, globals, frames, regs_vec, arg0..argN) -> (outcome, value)`. No
    /// `entry_pc` — a fast body is entered only fresh at pc 0; the arguments travel as machine
    /// arguments instead of window slots, and a completed `Return`'s value comes back as the
    /// second return word (`outcome == OUTCOME_RETURNED`).
    fn fast_abi_signature(&self, arity: usize) -> cranelift_codegen::ir::Signature {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut sig = self.module.make_signature();
        for _ in 0..6 {
            sig.params.push(AbiParam::new(ptr_ty));
        }
        for _ in 0..arity {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64)); // outcome
        sig.returns.push(AbiParam::new(types::I64)); // returned value (RETURNED only)
        sig
    }

    /// Finalize the current `self.ctx` under `name` and return its entry point.
    fn finalize(&mut self, name: &str) -> Result<CompiledFn, String> {
        // Debug tool: `NOETA_JIT_DISASM=1` dumps each compiled prototype's final machine code
        // (vcode form: post-regalloc, real machine instructions) to stderr — the native analogue
        // of `noeta dump`, for inspecting what the JIT actually emits.
        let want_disasm = std::env::var_os("NOETA_JIT_DISASM").is_some();
        self.ctx.set_disasm(want_disasm);
        let func_id = self
            .module
            .declare_function(name, Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        self.module
            .define_function(func_id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
        if want_disasm
            && let Some(code) = self.ctx.compiled_code()
            && let Some(vcode) = &code.vcode
        {
            eprintln!("=== {name} ===\n{vcode}");
        }
        self.module.clear_context(&mut self.ctx);
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        let code = self.module.get_finalized_function(func_id);
        // SAFETY: `code` is a finalized function whose Cranelift signature is exactly the
        // `extern "C" fn(ptr, ptr, usize) -> u32` this transmutes to, and it stays valid for as long
        // as `self.module` (which owns the code page) lives.
        Ok(unsafe { std::mem::transmute::<*const u8, CompiledFn>(code) })
    }

    /// Emit the bail stub for an ineligible prototype: call the `noeta_jit_observe` helper (proving
    /// the helper ABI links and the VM pointer round-trips) and return `0` — "interpret the whole
    /// frame".
    fn emit_bail_stub(&mut self, proto: usize) -> Result<CompiledFn, String> {
        self.module.clear_context(&mut self.ctx);
        self.ctx.func.signature = self.abi_signature();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fb_ctx);
            let block = b.create_block();
            b.append_block_params_for_function_params(block);
            b.switch_to_block(block);
            b.seal_block(block);
            let vm = b.block_params(block)[0];
            let helper_ref = self.module.declare_func_in_func(self.observe_id, b.func);
            b.ins().call(helper_ref, &[vm]);
            let zero = b.ins().iconst(types::I64, 0);
            b.ins().return_(&[zero]);
            b.finalize();
        }
        self.finalize(&format!("noeta_jit_stub{proto}"))
    }

    /// Emit the native integer body for a J1-eligible prototype (see the module docs). One Cranelift
    /// block per bytecode `pc`; register state lives in memory (the `regs` array), so blocks carry no
    /// SSA params — only the frame base pointer, computed once in the entry block, crosses into them.
    fn emit_int_body(
        &mut self,
        module: &Module,
        proto: usize,
        fast: bool,
    ) -> Result<CompiledFn, String> {
        let chunk = &module.protos[proto];
        let n = chunk.code.len();
        // A fast body is entered only fresh at pc 0 (no seam resume, no OSR — the interpreter
        // re-enters a deopted fast frame through the normal body).
        let reachable = if fast {
            reachable_pcs_from(chunk, vec![0])
        } else {
            reachable_pcs(chunk)
        };

        self.module.clear_context(&mut self.ctx);
        // S4.2 inline caches: one slot per call site in this body, allocated up front so their
        // (stable, boxed) addresses can be baked into the code below.
        let mut site_addrs: Vec<u64> = vec![0; n];
        for (pc, op) in chunk.code.iter().enumerate() {
            if matches!(op, Op::Call { .. } | Op::CallGlobal { .. }) {
                let slot: Box<CallSiteCache> = Box::new([SITE_EMPTY, 0, 0, 0]);
                site_addrs[pc] = slot.as_ref() as *const CallSiteCache as u64;
                self.site_slots.push(slot);
            }
        }
        let frame_template_addr = self.frame_template as u64;
        let layout = self.layout;
        // Precompute the ABI signature (also imported for the direct-call `call_indirect`) before the
        // builder borrows `self.ctx.func`, so it doesn't also need to borrow `self`.
        let abi_sig = self.abi_signature();
        self.ctx.func.signature = if fast {
            self.fast_abi_signature(chunk.num_params as usize)
        } else {
            abi_sig.clone()
        };
        let vec_len_off = (self.layout.vec_len_word * 8) as i32;
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fb_ctx);

            // The entry block holds the function params; one block per bytecode pc follows.
            let entry = b.create_block();
            let op_blocks: Vec<Block> = (0..n).map(|_| b.create_block()).collect();

            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let vm = b.block_params(entry)[0];
            let regs = b.block_params(entry)[1];
            let base = b.block_params(entry)[2];
            let globals = b.block_params(entry)[3];
            let frames = b.block_params(entry)[4];
            let regs_vec = b.block_params(entry)[5];
            // Normal body: param 6 is `entry_pc`. Fast body: params 6.. are the call arguments.
            let entry_pc = if fast {
                None
            } else {
                Some(b.block_params(entry)[6])
            };
            let arg_vals: Vec<ClValue> = if fast {
                (0..chunk.num_params as usize)
                    .map(|i| b.block_params(entry)[6 + i])
                    .collect()
            } else {
                Vec::new()
            };
            // frame_ptr = regs + base * 8 (Value is 8 bytes). All register access is off this.
            let base_bytes = b.ins().imul_imm(base, 8);
            let frame_ptr = b.ins().iadd(regs, base_bytes);
            let note_bound_ref = self.module.declare_func_in_func(self.note_bound_id, b.func);
            let retain_ref = self.module.declare_func_in_func(self.retain_id, b.func);
            let release_ref = self.module.declare_func_in_func(self.release_id, b.func);
            let release_value_ref = self
                .module
                .declare_func_in_func(self.release_value_id, b.func);
            let call_ref = self.module.declare_func_in_func(self.call_id, b.func);
            let return_ref = self.module.declare_func_in_func(self.return_id, b.func);
            let prepare_call_ref = self
                .module
                .declare_func_in_func(self.prepare_call_id, b.func);
            let after_call_ref = self.module.declare_func_in_func(self.after_call_id, b.func);
            let leaf_op_ref = self.module.declare_func_in_func(self.leaf_op_id, b.func);
            // The signature of a compiled prototype, imported so a direct call can `call_indirect`
            // another compiled prototype's entry point — plus one fast-convention signature per
            // distinct call arity in this chunk (S4.1).
            let callee_sig = b.import_signature(abi_sig.clone());
            let mut fast_sigs = std::collections::HashMap::new();
            for op in chunk.code.iter() {
                if let Op::Call { args, .. } | Op::CallGlobal { args, .. } = op {
                    let arity = args.len();
                    if let std::collections::hash_map::Entry::Vacant(e) = fast_sigs.entry(arity) {
                        e.insert(b.import_signature(fast_sig_for(&abi_sig, arity)));
                    }
                }
            }

            // A prototype that makes a call carries heap values (the callee closure, and results) in
            // registers, so its register writes must be refcount-correct (release the overwritten
            // value, retain a moved heap value). A call-free prototype keeps the immediate invariant
            // (J1/J2/globals) and the faster refcount-free stores — UNLESS it has an OSR entry: then
            // native execution can begin mid-frame (a loop header) with a heap value already live in a
            // register (the interpreter put it there), so the refcount-correct stores are mandatory,
            // just as for the resume-native (post-call) entry a call-bearing prototype already forces.
            let heap_aware = has_osr_entry(chunk)
                || chunk.code.iter().any(|op| {
                    matches!(op, Op::Call { .. } | Op::CallGlobal { .. }) || is_leaf_heap_op(op)
                });
            // The per-store-site release map (P-JIT bare stores): in a `heap_aware` prototype, a store
            // releases the overwritten value only where that value may be a heap pointer; where it is
            // provably an immediate the store is bare (skips the load-old + `is_pointer` release).
            let nreg = chunk.num_registers as usize;
            let heap_in = heap_in_map(chunk, heap_aware);
            let transfer = transfer_pairs(chunk);
            // P-JSSA: the register plan (per-pc liveness + S5 universal residency) and one SSA
            // variable per VM register. An unmodeled prototype promotes nothing (no variable is
            // ever defined or used) — its code is unchanged.
            let modeled = proto_modeled(chunk, heap_aware);
            let reg_plan = plan::RegPlan::with_heap_in(chunk, &heap_in, modeled);
            let const_bits = plan::const_reg_bits(chunk);
            let kinds = kind_in_map(chunk);
            // S5: with heap values SSA-resident, track where a slot may be heap-desynced from
            // its variable; the sync spill set is live ∪ hazard.
            let slot_hazard = if modeled && heap_aware {
                slot_hazard_map(chunk, &heap_in)
            } else {
                vec![false; n * nreg]
            };
            // Fast bodies need the must-slot-written map to normalize their (uninitialized)
            // window at every native exit; `fast_eligible` verified the contract holds.
            let must_written = if fast {
                must_slot_written_map(chunk, &const_bits)
            } else {
                Vec::new()
            };
            let vars: Vec<cranelift_frontend::Variable> =
                (0..nreg).map(|_| b.declare_var(types::I64)).collect();
            let raw_vars: Vec<cranelift_frontend::Variable> =
                (0..nreg).map(|_| b.declare_var(types::I64)).collect();

            let mut cg = Codegen {
                b: &mut b,
                vm,
                regs,
                frame_ptr,
                base,
                globals,
                frames,
                regs_vec,
                heap_aware,
                heap_in,
                nreg,
                transfer,
                cur_pc: 0,
                plan: reg_plan,
                vars,
                raw_vars,
                kinds,
                const_bits,
                proto: proto as u32,
                fast,
                must_written,
                slot_hazard,
                vec_len_off,
                layout,
                site_addrs,
                frame_template_addr,
                note_bound_ref,
                retain_ref,
                release_ref,
                release_value_ref,
                call_ref,
                return_ref,
                prepare_call_ref,
                after_call_ref,
                leaf_op_ref,
                callee_sig,
                fast_sigs,
            };

            if let Some(entry_pc) = entry_pc {
                // Entry-pc dispatch (J3 resume-native): jump to the block for `entry_pc`. `0` is a
                // fresh frame (run the parameter guard first); a post-call resume pc jumps straight
                // to its block; any other value has no native entry, so bail (the interpreter runs
                // that frame). The valid resume pcs are exactly `call_pc + 1` for each `Call` (the
                // interpreter re-enters a frame only at pc 0 or just after a call returns).
                let resume_targets: Vec<usize> =
                    entry_pcs(chunk).into_iter().filter(|&p| p != 0).collect();
                let guarded = cg.b.create_block();
                let bad_entry = cg.b.create_block();
                // Chain: entry_pc == 0 → guarded; == resume_pc_k → init_k → op_blocks[k]; else →
                // bad_entry. Every native entry point passes through an init block that loads the
                // SSA variables from the register slots (P-JSSA) — at an entry the interpreter's
                // slots are the truth — so every variable is defined on every path before any use.
                let is_zero = cg.b.ins().icmp_imm(IntCC::Equal, entry_pc, 0);
                let mut next = cg.b.create_block();
                let mut resume_inits: Vec<(usize, Block)> = Vec::new();
                cg.b.ins().brif(is_zero, guarded, &[], next, &[]);
                for (i, &rp) in resume_targets.iter().enumerate() {
                    cg.b.switch_to_block(next);
                    let is_rp = cg.b.ins().icmp_imm(IntCC::Equal, entry_pc, rp as i64);
                    let after = if i + 1 < resume_targets.len() {
                        cg.b.create_block()
                    } else {
                        bad_entry
                    };
                    let init = cg.b.create_block();
                    resume_inits.push((rp, init));
                    cg.b.ins().brif(is_rp, init, &[], after, &[]);
                    next = after;
                }
                if resume_targets.is_empty() {
                    cg.b.switch_to_block(next);
                    cg.b.ins().jump(bad_entry, &[]);
                }
                for (rp, init) in resume_inits {
                    cg.b.switch_to_block(init);
                    // Every mid-frame entry (seam resume or OSR header) verifies the analyses'
                    // immediate/kind claims against tier-0's actual slots before any native code
                    // trusts them — see `guard_entry_claims`.
                    cg.guard_entry_claims(rp);
                    cg.load_ssa_vars();
                    cg.init_raw_vars(rp);
                    cg.b.ins().jump(op_blocks[rp], &[]);
                }
                // `bad_entry`: an unexpected resume pc — hand the frame back to the interpreter
                // there. (`entry_pc` is pointer-width, i.e. i64 on the 64-bit target.)
                cg.b.switch_to_block(bad_entry);
                cg.b.ins().return_(&[entry_pc]);

                // `guarded` (fresh frame, entry_pc == 0): parameter guard, then the pc-0 init
                // block (SSA variables loaded from the guard-proven slots), then op block 0. If
                // any argument is a heap pointer, bail to pc 0 — keeping heap arguments out of
                // the body (the body's heap values then arise only from `LoadGlobal`/calls,
                // which the heap-aware path refcounts).
                cg.b.switch_to_block(guarded);
                let init0 = cg.b.create_block();
                let mut any_ptr: Option<ClValue> = None;
                for p in 0..chunk.num_params {
                    let v = cg.load_reg(p);
                    let is_ptr = cg.is_pointer(v);
                    any_ptr = Some(match any_ptr {
                        None => is_ptr,
                        Some(acc) => cg.b.ins().bor(acc, is_ptr),
                    });
                }
                match any_ptr {
                    Some(any) => {
                        let bail0 = cg.b.create_block();
                        cg.b.ins().brif(any, bail0, &[], init0, &[]);
                        cg.b.switch_to_block(bail0);
                        let zero = cg.b.ins().iconst(types::I64, 0);
                        cg.b.ins().return_(&[zero]);
                    }
                    None => {
                        cg.b.ins().jump(init0, &[]);
                    }
                }
                cg.b.switch_to_block(init0);
                cg.load_ssa_vars();
                cg.init_raw_vars(0);
                cg.b.ins().jump(op_blocks[0], &[]);
            } else {
                // Fast-convention entry (P-JSSA S4.1): the window is reserved but UNINITIALIZED
                // and the arguments arrive as machine arguments. Guard the argument *values* (no
                // slot loads); on a heap argument, materialize the window for the interpreter —
                // store each argument to its parameter slot with the retain the classic setup
                // would have done, unit-fill every other slot, and hand back pc 0. On the good
                // path the parameters' SSA variables are defined straight from the argument
                // values and the parameter slots are never written (they stay garbage; every
                // native exit runs `normalize_frame` before the interpreter can see them).
                let init0 = cg.b.create_block();
                let mut any_ptr: Option<ClValue> = None;
                for &a in &arg_vals {
                    let is_ptr = cg.is_pointer(a);
                    any_ptr = Some(match any_ptr {
                        None => is_ptr,
                        Some(acc) => cg.b.ins().bor(acc, is_ptr),
                    });
                }
                match any_ptr {
                    Some(any) => {
                        let bail0 = cg.b.create_block();
                        cg.b.ins().brif(any, bail0, &[], init0, &[]);
                        cg.b.switch_to_block(bail0);
                        let unit =
                            cg.b.ins()
                                .iconst(types::I64, Value::NANBOX.unit_bits as i64);
                        for (pi, &a) in arg_vals.iter().enumerate() {
                            cg.b.ins().store(
                                MemFlagsData::trusted(),
                                a,
                                cg.frame_ptr,
                                reg_offset(pi as Reg),
                            );
                            cg.retain_if_heap(a);
                        }
                        for r in chunk.num_params..nreg as u16 {
                            cg.b.ins().store(
                                MemFlagsData::trusted(),
                                unit,
                                cg.frame_ptr,
                                reg_offset(r),
                            );
                        }
                        let zero = cg.b.ins().iconst(types::I64, 0);
                        cg.b.ins().return_(&[zero, zero]);
                    }
                    None => {
                        cg.b.ins().jump(init0, &[]);
                    }
                }
                cg.b.switch_to_block(init0);
                let unit =
                    cg.b.ins()
                        .iconst(types::I64, Value::NANBOX.unit_bits as i64);
                for r in 0..nreg {
                    let v = arg_vals.get(r).copied().unwrap_or(unit);
                    cg.b.def_var(cg.vars[r], v);
                }
                cg.init_raw_vars(0);
                cg.b.ins().jump(op_blocks[0], &[]);
            }

            // One block per op. Unreachable pcs (dead code) get a trivial bail so they never touch
            // `frame_ptr` (which only dominates reachable blocks).
            for (pc, op) in chunk.code.iter().enumerate() {
                cg.b.switch_to_block(op_blocks[pc]);
                if !reachable[pc] {
                    let here = cg.b.ins().iconst(types::I64, pc as i64);
                    cg.ret_bail(here);
                    continue;
                }
                emit_op(&mut cg, &chunk.consts, op, pc, &op_blocks);
            }

            b.seal_all_blocks();
            b.finalize();
        }
        let tag = if fast { "fast" } else { "proto" };
        self.finalize(&format!("noeta_jit_{tag}{proto}"))
    }
}

/// Build the fast-convention signature for `arity` from the normal ABI signature's pointer
/// params (P-JSSA S4.1): the six fixed pointers, then `arity` NaN-boxed `i64` arguments, and the
/// two-word (outcome, value) return. Must agree with [`Jit::fast_abi_signature`].
fn fast_sig_for(
    abi_sig: &cranelift_codegen::ir::Signature,
    arity: usize,
) -> cranelift_codegen::ir::Signature {
    let mut sig = abi_sig.clone();
    sig.params.truncate(6); // drop entry_pc
    for _ in 0..arity {
        sig.params.push(AbiParam::new(types::I64));
    }
    sig.returns.push(AbiParam::new(types::I64)); // second word: the returned value
    sig
}

/// Whether a prototype is worth compiling: it has at least one op the JIT emits natively. Ops it
/// doesn't (calls, heap ops, `Echo`, `Return`, `Halt`, …) are *bail points* — the body runs its
/// compilable ops and hands back to the interpreter at the first one it can't (per-op bail). A
/// prototype with no fast op at all gets a bail stub instead (nothing to gain).
fn is_eligible(chunk: &noeta_bytecode::Chunk) -> bool {
    chunk.code.iter().any(|op| is_fast_op(op, &chunk.consts))
}

/// Whether [`emit_op`] compiles this op instance to native code (vs bailing to the interpreter at it).
/// A `LoadConst` is fast only if its constant is an immediate (int in the 48-bit range, bool, unit, or
/// a float — never a heap string/module or a big int that would box); a `Binary` only for the integer/
/// float arithmetic-and-comparison set.
fn is_fast_op(op: &Op, consts: &[Const]) -> bool {
    match op {
        Op::LoadConst { k, .. } => const_immediate_bits(&consts[*k as usize]).is_some(),
        Op::Move { .. } | Op::Drop { .. } => true,
        Op::Binary { op, .. } => supported_binary(*op),
        Op::LoadGlobal { .. } | Op::StoreGlobal { .. } | Op::TakeGlobal { .. } => true,
        Op::Call { .. } | Op::CallGlobal { .. } | Op::Return { .. } => true,
        Op::Jump { .. }
        | Op::JumpIfTrue { .. }
        | Op::JumpIfFalse { .. }
        | Op::CondBranch { .. } => true,
        op if is_leaf_heap_op(op) => true,
        _ => false,
    }
}

/// The leaf heap/collection ops the JIT runs through the `run_leaf_op` helper (J4) — non-dispatching
/// ops whose exact interpreter logic (refcounts included) the helper reproduces, bailing on the
/// dispatch/error cases. A prototype containing one is `heap_aware` (they can put a heap value in a
/// register).
fn is_leaf_heap_op(op: &Op) -> bool {
    matches!(
        op,
        Op::MakeRange { .. }
            | Op::IterSnapshot { .. }
            | Op::ListLen { .. }
            | Op::ListGet { .. }
            | Op::LoadField { .. }
            | Op::SetField { .. }
            | Op::Index { .. }
            | Op::MakeTuple { .. }
            | Op::TupleIndex { .. }
    )
}

/// The binary operators J1 compiles natively: integer arithmetic and comparison. (Bitwise/shift and
/// the fixed-width `WideInt` ops are a later slice; `~`/identity/logical are not integer ops.)
fn supported_binary(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
    )
}

/// The exact NaN-box bits a constant materializes to, if it is an immediate; `None` for a heap
/// constant (string / native module) or an out-of-range integer that would box on the heap.
fn const_immediate_bits(c: &Const) -> Option<u64> {
    match c {
        Const::Unit => Some(Value::unit().bits()),
        Const::Bool(bl) => Some(Value::bool(*bl).bits()),
        Const::Int(i) => {
            let l = Value::NANBOX;
            if (l.int_min..=l.int_max).contains(i) {
                Some(l.qnan | l.int_tag | (*i as u64 & l.ptr_mask))
            } else {
                None // big int → heap-boxed, not an immediate
            }
        }
        Const::Float(f) => Some(Value::float(*f).bits()),
        Const::F32(f) => Some(Value::f32(*f).bits()),
        Const::Str(_)
        | Const::NativeModule(_)
        | Const::ModuleFn { .. }
        | Const::MethodHandle { .. } => None,
    }
}

/// The bytecode pcs at which the interpreter may (re-)enter this prototype's native code: pc 0 (a
/// fresh frame), every `call_pc + 1` (a resume after a native `Call` returned — J3 resume-native),
/// and every **loop header** (a backward-branch target — J5 OSR). Those are the only pcs a frame's
/// saved `pc` ever holds when the interpreter re-enters at a `'reload` transition. The loop headers
/// are what let a long-running loop enter tier 1 *mid-frame* (on-stack replacement) rather than only
/// at a call boundary — closing the hole where a top-level loop (entered once) never gets hot.
fn entry_pcs(chunk: &noeta_bytecode::Chunk) -> Vec<usize> {
    let n = chunk.code.len();
    let mut pcs = vec![0usize];
    for (pc, op) in chunk.code.iter().enumerate() {
        if matches!(op, Op::Call { .. } | Op::CallGlobal { .. }) && pc + 1 < n {
            pcs.push(pc + 1);
        }
        if let Some(t) = backward_target(op, pc) {
            pcs.push(t);
        }
    }
    pcs.sort_unstable();
    pcs.dedup();
    pcs
}

/// The target of a **backward** branch at `pc` (a loop back-edge), or `None` if `op` is not a branch
/// or branches forward. A backward-branch target is a loop header — an OSR (on-stack replacement)
/// entry point (J5). `target <= pc` is the back-edge test (a self-loop `target == pc` counts).
fn backward_target(op: &Op, pc: usize) -> Option<usize> {
    let t = match op {
        Op::Jump { target }
        | Op::JumpIfTrue { target, .. }
        | Op::JumpIfFalse { target, .. }
        | Op::CondBranch { target, .. } => *target as usize,
        _ => return None,
    };
    (t <= pc).then_some(t)
}

/// Whether this prototype has any OSR (loop-header) entry point — a backward branch (J5). Such a
/// prototype is compiled `heap_aware` unconditionally: OSR enters mid-frame with whatever the
/// interpreter left in the registers (a heap value may be live), so every store must be
/// refcount-correct, exactly the precondition J3 resume-native already relies on.
fn has_osr_entry(chunk: &noeta_bytecode::Chunk) -> bool {
    chunk
        .code
        .iter()
        .enumerate()
        .any(|(pc, op)| backward_target(op, pc).is_some())
}

/// Whether OSR-compiling this prototype is worthwhile: it has at least one loop whose body native
/// code can **sustain** — every op between a loop header and its back-edge compiles to native code
/// ([`is_fast_op`]). A loop whose body contains a bail op exits native on the first such op *every*
/// iteration (a tier-0↔tier-1 bounce that costs more than just interpreting the loop), so a prototype
/// whose only loops bail is left in the interpreter. The loop body is over-approximated as the pc
/// range `[header, back_edge]` — conservative (an occasional bail in a rarely-taken branch declines a
/// loop that is mostly native-able), which errs toward not regressing a heap-op-dominated loop.
pub fn worth_osr(chunk: &noeta_bytecode::Chunk) -> bool {
    let code = &chunk.code;
    code.iter().enumerate().any(|(pc, op)| {
        backward_target(op, pc).is_some_and(|header| {
            code[header..=pc]
                .iter()
                .all(|o| is_fast_op(o, &chunk.consts))
        })
    })
}

/// Whether compiling this prototype is worthwhile at all (the entry path *and* OSR). A loopless
/// prototype — a recursive function like `fib`, straight-line code — is worth compiling: it runs its
/// body once per activation with no per-iteration bail bounce. A prototype *with* a loop is worth it
/// only if some loop is native-sustainable ([`worth_osr`]); one whose every loop bails would bounce
/// tier-0↔tier-1 every iteration, slower than just interpreting it.
pub fn worth_compiling(chunk: &noeta_bytecode::Chunk) -> bool {
    !has_osr_entry(chunk) || worth_osr(chunk)
}

/// Forward reachability of each bytecode pc in the *native* control-flow graph, seeded from every
/// native entry point ([`entry_pcs`]) — a fresh frame (pc 0) and every post-call resume pc. A non-fast
/// op is terminal — it bails (returns its pc), so it has no native successor — which is why this
/// follows edges only out of fast ops. Used so the codegen fills unreachable blocks (dead code, or the
/// fall-through past a bail) with a trivial bail instead of code that would reference the entry-only
/// frame/globals pointers from a non-dominated block.
fn reachable_pcs(chunk: &noeta_bytecode::Chunk) -> Vec<bool> {
    reachable_pcs_from(chunk, entry_pcs(chunk))
}

/// [`reachable_pcs`] seeded from an explicit entry set (the fast body's is just `{0}`).
fn reachable_pcs_from(chunk: &noeta_bytecode::Chunk, entries: Vec<usize>) -> Vec<bool> {
    let n = chunk.code.len();
    let mut seen = vec![false; n];
    let mut stack = entries;
    while let Some(pc) = stack.pop() {
        if pc >= n || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let op = &chunk.code[pc];
        if !is_fast_op(op, &chunk.consts) {
            continue; // a bail point: no native successor
        }
        match op {
            Op::Jump { target } => stack.push(*target as usize),
            Op::JumpIfTrue { target, .. } | Op::JumpIfFalse { target, .. } => {
                stack.push(*target as usize);
                stack.push(pc + 1);
            }
            Op::CondBranch { target, .. } => {
                stack.push(*target as usize);
                stack.push(pc + 1);
            }
            // A native `Call`/`CallGlobal` continues at `pc + 1` on the direct/fast path
            // (J3/S4.1) — this edge is what lets a fast body run its post-call ops natively
            // instead of compiling them to bail fillers. `Return` ends the frame.
            Op::Call { .. } | Op::CallGlobal { .. } => stack.push(pc + 1),
            Op::Return { .. } => {}
            _ => stack.push(pc + 1), // fast straight-line op
        }
    }
    seen
}

/// Whether a `Binary`'s result — as produced by *native code* — is always an immediate. Used by the
/// bare-store analysis to decide whether a `Binary`'s destination may hold a heap value afterwards.
///
/// A comparison (`==`/`<`/…) or short-circuit `&&`/`||` yields a bool. **Arithmetic yields an
/// immediate too, in the JIT:** the native `Binary` guards the 48-bit range and *bails to the
/// interpreter before storing* on an overflowing (would-be heap-boxed) integer result, and the float
/// path is NaN-boxed — so a register holding a *completed* native arithmetic result is provably
/// immediate at every point the interpreter can re-enter (the boxing case already left native code).
/// `~` (`Concat`) and other heap-building ops stay may-heap.
fn binary_result_is_immediate(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Identity
            | BinaryOp::NotIdentity
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
    )
}

/// One op's effect on the per-register "may hold a heap value" set (the bare-store analysis,
/// [`heap_in_map`]). `None` means the op is *not modeled* — its effect on registers is unknown to
/// this analysis — which opts the whole prototype out (the analysis fails **closed**: every store
/// keeps its refcount-correct release). Only the ops that appear in a pure-arithmetic loop are
/// modeled; a call, a leaf/heap op, a closure, an index — anything richer — returns `None`, so the
/// optimization is confined to exactly the prototypes where it was measured to help and cannot silently
/// mis-model a heap value.
enum RegEffect {
    /// Reads/writes no register the analysis tracks (a branch, `Echo`, `Return`, `Halt`).
    Inert,
    /// The op leaves `reg` holding `unit` (a `Drop`, or a `StoreGlobal` that moves its source out).
    Clear(Reg),
    /// `dst` is (re)defined; `heap` = whether its new value may be a heap pointer.
    Def { dst: Reg, heap: bool },
    /// `dst = src` — `dst` inherits `src`'s heap-ness (a `Move`).
    Copy { dst: Reg, src: Reg },
}

/// Model one op's effect on the may-hold-heap set, or `None` if the op is unmodeled (see
/// [`RegEffect`]). The whitelist is deliberately the pure-arithmetic-loop subset.
fn reg_effect(op: &Op, consts: &[Const]) -> Option<RegEffect> {
    Some(match op {
        Op::LoadConst { dst, k } => RegEffect::Def {
            dst: *dst,
            heap: const_immediate_bits(&consts[*k as usize]).is_none(),
        },
        Op::Move { dst, src } => RegEffect::Copy {
            dst: *dst,
            src: *src,
        },
        Op::Drop { reg, .. } => RegEffect::Clear(*reg),
        Op::StoreGlobal { src, .. } => RegEffect::Clear(*src),
        Op::LoadGlobal { dst, .. } | Op::TakeGlobal { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        Op::Binary { op, dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: !binary_result_is_immediate(*op),
        },
        Op::Unary { dst, .. } | Op::Stringify { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        // A call redefines only its destination (the result, which may be a heap value) and reads its
        // args; from the *caller's* register view nothing else changes. Modeling it — rather than
        // failing the whole map closed — lets a call-bearing prototype (every recursive/helper fn)
        // bare-store its provably-immediate arithmetic temps. The interpreter's re-entry after a call
        // that bails resumes at `pc + 1` with exactly this state (result in `dst`, other caller
        // registers preserved), so the forward dataflow's out-set there over-approximates it.
        Op::Call { dst, .. } | Op::CallGlobal { dst, .. } => RegEffect::Def {
            dst: *dst,
            heap: true,
        },
        Op::Jump { .. }
        | Op::JumpIfTrue { .. }
        | Op::JumpIfFalse { .. }
        | Op::CondBranch { .. }
        | Op::RequireBool { .. }
        | Op::RequireCondBool { .. }
        | Op::Echo { .. }
        | Op::Return { .. }
        | Op::Halt => RegEffect::Inert,
        _ => return None,
    })
}

/// The analysis CFG successors of `pc` under *tier-0* (interpreter) semantics — the paths along which a
/// register's value flows. Every op falls through to `pc + 1` except a jump (to its target), a
/// conditional branch (both), and the terminators `Return`/`Halt` (none). Modeled only for the
/// whitelisted ops ([`reg_effect`]); the caller has already rejected any other op.
fn analysis_succ(op: &Op, pc: usize, n: usize, out: &mut Vec<usize>) {
    out.clear();
    match op {
        Op::Jump { target } => out.push(*target as usize),
        Op::JumpIfTrue { target, .. }
        | Op::JumpIfFalse { target, .. }
        | Op::CondBranch { target, .. } => {
            out.push(*target as usize);
            if pc + 1 < n {
                out.push(pc + 1);
            }
        }
        Op::Return { .. } | Op::Halt => {}
        _ => {
            if pc + 1 < n {
                out.push(pc + 1);
            }
        }
    }
}

/// The bare-store release map for a prototype: `map[pc * nreg + r]` is whether a store to register `r`
/// at `pc` must release the value it overwrites. A `false` cell means the old occupant is provably an
/// immediate, so the store can skip the load-old + `is_pointer` release (the bare-store optimization).
///
/// - A non-`heap_aware` prototype already stores bare everywhere (the immediate invariant) → all-false.
/// - A `heap_aware` prototype the forward analysis can model → the precise may-hold-heap set at each
///   store site (release iff the overwritten value may be a heap pointer).
/// - A `heap_aware` prototype with any unmodeled op → all-true (release everywhere, the prior behavior).
///
/// **Soundness.** The analysis is a monotone forward may-hold-heap dataflow over the tier-0 CFG
/// (join = union), seeded all-immediate at entry (`pc 0`): locals are `unit`-initialized and the
/// parameters' immediate claim is **runtime-verified** — the pc-0 entry guard bails on a heap
/// argument before the body runs, and every mid-frame entry re-verifies the claims
/// ([`Codegen::guard_entry_claims`]), the same contract that covers the natively-stored-result
/// claims. A `false` cell is thus a *guarantee* the register holds an immediate wherever native
/// code executes, so skipping the release is a no-op — the interpreter's `set_reg` would release
/// an immediate, which is itself a no-op. Every modeled op's [`RegEffect`] only clears a bit
/// (`Drop`/`StoreGlobal` leave `unit`), copies one (`Move`), or sets one to an over-approximation
/// of its result's heap-ness; an unmodeled op fails the whole map closed.
pub(crate) fn heap_in_map(chunk: &noeta_bytecode::Chunk, heap_aware: bool) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    if !heap_aware {
        return vec![false; n * nreg];
    }
    match heap_at_fixpoint(chunk, n, nreg) {
        Some(inset) => inset,
        None => vec![true; n * nreg],
    }
}

/// Whether the register-effect model covers this prototype (P-JSSA S5's residency permission):
/// either every op is modeled by [`reg_effect`], or the prototype is call-free/non-OSR
/// (`!heap_aware`), where the pc-0 entry guard's all-immediate invariant holds without any
/// modeling — every register write is then refcount-free by construction, and unmodeled ops are
/// plain bail points whose sync uses the (always-modeled) liveness.
pub(crate) fn proto_modeled(chunk: &noeta_bytecode::Chunk, heap_aware: bool) -> bool {
    !heap_aware
        || chunk
            .code
            .iter()
            .all(|op| reg_effect(op, &chunk.consts).is_some())
}

/// The forward **slot-hazard** fixpoint (P-JSSA S5): `map[pc * nreg + r]` = may register `r`'s
/// window slot be **out of sync with its variable in a heap-relevant way** at the start of `pc`?
/// With heap values SSA-resident, a def releases the old value *from the variable* and writes no
/// slot — so a slot can hold a released (dangling) pointer, or fail to hold the heap reference
/// the register owns. Either way the slot must be re-synced before anything that reads or
/// releases it (teardown, unwind, the interpreter). The spill set at every sync point is
/// therefore `live ∪ hazard`.
///
/// Transfer: a def/clear of `r` raises the hazard if the *old* value may be heap
/// (`heap_in[pc][r]` — its release/move leaves released bits in the slot) or the *new* value may
/// be heap (`heap_in[pc+1][r]` — the variable now owns a reference the slot doesn't hold). A
/// call clears everything (the pre-call sync spilled `live ∪ hazard`, and immediates are
/// stale-safe) and then raises its own destination (the fast path writes the result to the
/// variable only). Entries seed in-sync (variables are loaded from the slots). Join = OR.
pub(crate) fn slot_hazard_map(chunk: &noeta_bytecode::Chunk, heap_in: &[bool]) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let all_immediate = heap_in.iter().all(|&h| !h);
    if all_immediate {
        return vec![false; n * nreg]; // stale slots are stale-immediates — always safe
    }
    let heap_at = |pc: usize, r: usize| -> bool {
        let i = pc * nreg + r;
        i < heap_in.len() && heap_in[i]
    };
    let mut hazard = vec![false; n * nreg];
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out: Vec<bool> = hazard[pc * nreg..pc * nreg + nreg].to_vec();
            match reg_effect(&chunk.code[pc], &chunk.consts) {
                Some(RegEffect::Def { dst, .. }) | Some(RegEffect::Copy { dst, .. }) => {
                    let d = dst as usize;
                    if matches!(chunk.code[pc], Op::Call { .. } | Op::CallGlobal { .. }) {
                        out.fill(false); // the pre-call sync spilled live ∪ hazard
                        out[d] = true; // the result lands in the variable only (fast path)
                    } else {
                        out[d] = out[d] || heap_at(pc, d) || heap_at(pc + 1, d);
                    }
                }
                Some(RegEffect::Clear(r)) => {
                    let d = r as usize;
                    out[d] = out[d] || heap_at(pc, d);
                }
                Some(RegEffect::Inert) => {}
                None => return vec![true; n * nreg], // unmodeled → no residency anyway
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &su in &succ {
                let base = su * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if o && !hazard[base + r] {
                        hazard[base + r] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    hazard
}

/// The ownership-transfer peephole map for a prototype (see [`Codegen::transfer`]): pc `i` is marked
/// when a `Move dst <- src` at `i` is immediately followed by a `Drop src` at `i + 1` — an ownership
/// transfer whose retain/release pair cancels. Both `i` and `i + 1` are marked. The `Drop` must be
/// reachable *only* through the `Move` (no branch targets `i + 1`), so that on every path reaching the
/// `Drop` the `Move` ran first and the pairing holds; a `Drop` that is a jump target is left alone.
fn transfer_pairs(chunk: &noeta_bytecode::Chunk) -> Vec<bool> {
    let n = chunk.code.len();
    let mut is_target = vec![false; n + 1];
    for op in &chunk.code {
        match op {
            Op::Jump { target }
            | Op::JumpIfTrue { target, .. }
            | Op::JumpIfFalse { target, .. }
            | Op::CondBranch { target, .. } => is_target[*target as usize] = true,
            _ => {}
        }
    }
    let mut transfer = vec![false; n];
    for pc in 0..n {
        let Op::Move { src, .. } = chunk.code[pc] else {
            continue;
        };
        if pc + 1 < n
            && !is_target[pc + 1]
            && matches!(chunk.code[pc + 1], Op::Drop { reg, .. } if reg == src)
        {
            transfer[pc] = true;
            transfer[pc + 1] = true;
        }
    }
    transfer
}

/// The forward may-hold-heap fixpoint used by [`heap_in_map`], or `None` if any op is unmodeled.
/// Returns the in-set flattened as `[pc * nreg + r]`: whether register `r` may hold a heap value at the
/// *start* of op `pc` — which is exactly whether a store to `r` at `pc` overwrites a possibly-heap
/// value.
fn heap_at_fixpoint(chunk: &noeta_bytecode::Chunk, n: usize, nreg: usize) -> Option<Vec<bool>> {
    let effects: Vec<RegEffect> = chunk
        .code
        .iter()
        .map(|op| reg_effect(op, &chunk.consts))
        .collect::<Option<_>>()?;

    // in[pc * nreg + r]: may register `r` hold a heap value at the start of op `pc`. Seed:
    // all-immediate — locals are `unit`-initialized, and the parameters are **claimed**
    // immediate too (T1b): the pc-0 entry guard has always bailed on a heap argument before the
    // body runs, and every mid-frame entry verifies the claim (below), so a parameter register
    // is promotable — and typed-readable — like any other, until an op redefines it as may-heap.
    let mut inset = vec![false; n * nreg];
    // NOTE (soundness, P-JSSA): this forward model describes tier-0's *fall-through* state, but a
    // mid-frame native entry (a seam resume after an interpreted callee, or an OSR loop header)
    // begins with whatever tier 0 actually left in the slots — and tier 0 can put a heap value
    // where this model claims an immediate (a heap argument reaches the body when tier 0 runs the
    // frame; an overflowing arithmetic result heap-boxes to a big int, where native bails
    // *before* such a store — so the discrepancy arises exactly when tier 0 ran the segment). A
    // false immediate claim would skip a needed retain/release — a leak, or a double-release.
    // Rather than dropping the claims (which would forfeit post-call bare stores and the loop
    // promotion), every native entry **verifies** them at runtime and bails on a violation — the
    // pc-0 parameter guard for a fresh frame, `Codegen::guard_entry_claims` for every mid-frame
    // entry. Native→native direct calls never pass through a mid-frame entry (the callee provably
    // ran fully native), so the hot path pays nothing.

    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            // out = transfer(in[pc], effects[pc]).
            let mut out = inset[pc * nreg..pc * nreg + nreg].to_vec();
            match effects[pc] {
                RegEffect::Inert => {}
                RegEffect::Clear(r) => out[r as usize] = false,
                RegEffect::Def { dst, heap } => out[dst as usize] = heap,
                RegEffect::Copy { dst, src } => out[dst as usize] = out[src as usize],
            }
            // Propagate out into each successor's in-set (join = OR).
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &s in &succ {
                let base = s * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if o && !inset[base + r] {
                        inset[base + r] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    Some(inset)
}

/// A register's statically-known **immediate kind** (P-JSSA T1 typed promotion). Where the kind
/// analysis ([`kind_in_map`]) proves a register `Int`/`Bool`/`Float` at a pc, the codegen skips
/// the NaN-box tag checks and (for `Int`/`Bool`) works on a second, **raw** SSA variable holding
/// the unboxed value — the box/unbox chain the egraph cannot fold through loop-header block
/// params disappears from the loop body entirely.
///
/// The lattice: `Bot < {Int, Bool, Float} < Imm`, join = equality-or-`Imm`. A typed kind is a
/// *claim* with the same contract as the bare-store analysis's immediate claims: true along
/// native paths by construction (a native int `Binary` bails before storing a non-fitting
/// result), and **verified at every mid-frame entry** against tier-0's actual slots
/// ([`Codegen::guard_entry_claims`]) — tier 0 can heap-box an overflow where the claim says
/// `Int`, and the guard catches exactly that (subsuming the plain `is_pointer` check).
///
/// Invariant (locked by a test): a typed kind implies the bare-store analysis proves the
/// register immediate there (`kind ∈ {Int, Bool, Float}` ⇒ `!heap_in`) — both transfers mark
/// the same may-heap defs, so a kind claim never outlives its immediate claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// No analyzed path has defined the register yet (an unreached cell) — the join identity.
    Bot,
    /// A small int (48-bit immediate). Raw form: the sign-extended i64.
    Int,
    /// A bool. Raw form: 0 or 1 in an i64.
    Bool,
    /// An f64. No raw form — a NaN-boxed float's word *is* its bit pattern; the win is skipping
    /// the type dispatch, not the (identity) unboxing.
    Float,
    /// An immediate of statically-unknown kind (or a cell outside the promoted region).
    Imm,
}

impl Kind {
    fn join(self, other: Kind) -> Kind {
        match (self, other) {
            (Kind::Bot, k) | (k, Kind::Bot) => k,
            (a, b) if a == b => a,
            _ => Kind::Imm,
        }
    }
}

/// The kind a `LoadConst` of this constant leaves in its destination. Heap constants (strings,
/// big ints, native modules) are `Imm` — irrelevant, since their defs are also may-heap in the
/// bare-store analysis, so no typed claim survives past them.
fn const_kind(c: &Const) -> Kind {
    match const_immediate_bits(c) {
        None => Kind::Imm,
        Some(_) => match c {
            Const::Int(_) => Kind::Int,
            Const::Bool(_) => Kind::Bool,
            Const::Float(_) => Kind::Float,
            _ => Kind::Imm, // unit, f32
        },
    }
}

/// The kind of a statically-known immediate word (a [`plan::const_reg_bits`] constant register).
fn classify_immediate_bits(bits: u64) -> Kind {
    let l = Value::NANBOX;
    if bits & (l.sign_bit | l.qnan | l.int_tag) == l.qnan | l.int_tag {
        Kind::Int
    } else if bits == l.true_bits || bits == l.false_bits {
        Kind::Bool
    } else if bits & l.qnan != l.qnan {
        Kind::Float
    } else {
        Kind::Imm // unit, f32
    }
}

/// The forward kind fixpoint (T1 typed promotion): `map[pc * nreg + r]` = register `r`'s
/// statically-known kind at the *start* of op `pc`, over the same tier-0 CFG and the same
/// modeled-op whitelist as the bare-store analysis ([`reg_effect`] — any unmodeled op fails the
/// whole map closed to `Imm`, which emits exactly today's generic code). Transfer highlights:
/// a comparison defines `Bool`; arithmetic defines `Int`/`Float` only when *both* operands
/// already have that kind (else `Imm` — tier 0 may coerce or produce either); everything
/// may-heap (params at entry, globals, call results, heap constants) defines `Imm`, keeping the
/// typed⇒immediate invariant aligned with [`heap_at_fixpoint`] by construction.
pub(crate) fn kind_in_map(chunk: &noeta_bytecode::Chunk) -> Vec<Kind> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let all_imm = || vec![Kind::Imm; n * nreg];
    if chunk
        .code
        .iter()
        .any(|op| reg_effect(op, &chunk.consts).is_none())
    {
        return all_imm();
    }
    let mut inset = vec![Kind::Bot; n * nreg];
    // Seed pc 0: parameters hold caller values of unknown kind; locals hold `unit`. All `Imm`.
    let row0 = nreg.min(inset.len());
    inset[..row0].fill(Kind::Imm);
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out = inset[pc * nreg..pc * nreg + nreg].to_vec();
            match &chunk.code[pc] {
                Op::LoadConst { dst, k } => {
                    out[*dst as usize] = const_kind(&chunk.consts[*k as usize]);
                }
                Op::Move { dst, src } => out[*dst as usize] = out[*src as usize],
                Op::Drop { reg, .. } => out[*reg as usize] = Kind::Imm, // leaves `unit`
                Op::StoreGlobal { src, .. } => out[*src as usize] = Kind::Imm,
                Op::LoadGlobal { dst, .. }
                | Op::TakeGlobal { dst, .. }
                | Op::Call { dst, .. }
                | Op::CallGlobal { dst, .. }
                | Op::Unary { dst, .. }
                | Op::Stringify { dst, .. } => out[*dst as usize] = Kind::Imm,
                Op::Binary { op, dst, a, b, .. } => {
                    out[*dst as usize] = match op {
                        BinaryOp::Eq
                        | BinaryOp::Ne
                        | BinaryOp::Lt
                        | BinaryOp::Le
                        | BinaryOp::Gt
                        | BinaryOp::Ge => Kind::Bool,
                        BinaryOp::Add
                        | BinaryOp::Sub
                        | BinaryOp::Mul
                        | BinaryOp::Div
                        | BinaryOp::Rem => match (out[*a as usize], out[*b as usize]) {
                            (Kind::Int, Kind::Int) => Kind::Int,
                            (Kind::Float, Kind::Float) => Kind::Float,
                            _ => Kind::Imm,
                        },
                        _ => Kind::Imm, // `~`, `&&`/`||`, identity — bail ops here; tier 0 decides
                    };
                }
                // Inert for registers (the reg_effect whitelist guarantees nothing else appears).
                _ => {}
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &s in &succ {
                let base = s * nreg;
                for (r, &o) in out.iter().enumerate() {
                    let j = inset[base + r].join(o);
                    if j != inset[base + r] {
                        inset[base + r] = j;
                        changed = true;
                    }
                }
            }
        }
    }
    inset
}

/// The forward **must-slot-written** fixpoint (P-JSSA S4.1): `map[pc * nreg + r]` = has register
/// `r`'s window *slot* been written on **every** path from a fresh pc-0 entry to the start of op
/// `pc`? Under the fast call convention the callee's window is reserved without initialization,
/// so `normalize_frame` may keep a slot's contents only where this map proves a real store
/// happened. With S5's universal residency the only defs that write slots are the
/// known-constant registers' `LoadConst`s (everything else is a pure variable def); sync spills
/// also write slots, but path-dependently, so they are not counted. Meet = AND over
/// [`analysis_succ`] predecessors; pc 0 starts all-unwritten. Requires every op modeled by
/// [`reg_effect`] (the caller checks [`fast_ok`] first).
pub(crate) fn must_slot_written_map(
    chunk: &noeta_bytecode::Chunk,
    const_bits: &[Option<u64>],
) -> Vec<bool> {
    let n = chunk.code.len();
    let nreg = chunk.num_registers as usize;
    let slot_write = |r: usize| -> bool { const_bits.get(r).is_some_and(|c| c.is_some()) };
    // Must-analysis: cells start "written" (⊤) except the entry row, and intersect over
    // predecessors; iterate to the greatest fixpoint. `written[pc]` describes the start of `pc`.
    let mut written = vec![true; n * nreg];
    let row0 = nreg.min(written.len());
    written[..row0].fill(false);
    let mut succ = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for pc in 0..n {
            let mut out: Vec<bool> = written[pc * nreg..pc * nreg + nreg].to_vec();
            match reg_effect(&chunk.code[pc], &chunk.consts) {
                Some(RegEffect::Def { dst, .. }) | Some(RegEffect::Copy { dst, .. }) => {
                    let d = dst as usize;
                    out[d] = out[d] || slot_write(d);
                }
                Some(RegEffect::Clear(r)) => {
                    let d = r as usize;
                    out[d] = out[d] || slot_write(d);
                }
                Some(RegEffect::Inert) => {}
                None => unreachable!("fast_ok requires a fully modeled prototype"),
            }
            analysis_succ(&chunk.code[pc], pc, n, &mut succ);
            for &su in &succ {
                let base = su * nreg;
                for (r, &o) in out.iter().enumerate() {
                    if !o && written[base + r] {
                        written[base + r] = false;
                        changed = true;
                    }
                }
            }
        }
    }
    written
}

/// Whether a prototype is eligible for the **fast call convention** (P-JSSA S4.1/S5): entered
/// with its window reserved **uninitialized** and its arguments as machine arguments, so every
/// native exit must make the window fully tier-0-valid (`Codegen::normalize_frame`). With S5's
/// universal residency the requirements reduce to:
///
/// - every op modeled by the register-effect whitelist (unknown slot effects are unnormalizable);
/// - ≤ 64 registers (the teardown mask, and bounded normalize code).
///
/// Reads-before-write need no clause: reads go through the variables, which the fast entry
/// initializes to `unit` — exactly tier-0's fresh-local semantics. Slot validity needs no
/// per-return clause: the fast return releases from the variables, and `normalize_frame` spills
/// `live ∪ hazard` and unit-fills everything not must-written.
pub(crate) fn fast_ok(chunk: &noeta_bytecode::Chunk) -> bool {
    chunk.num_registers <= 64
        && chunk
            .code
            .iter()
            .all(|op| reg_effect(op, &chunk.consts).is_some())
}

/// A thin wrapper over the Cranelift builder carrying the frame base pointer, the globals base
/// pointer, and the `note_global_bound` helper reference — the context every op-emitter needs. Keeps
/// [`emit_op`] free of builder plumbing. Register/global access uses *trusted* memory flags (aligned
/// — both `Vec<Value>`s are 8-byte aligned and every slot is at an 8-byte offset — and non-trapping,
/// since the compiler proved every register/slot in range), so Cranelift emits a bare load/store.
struct Codegen<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    /// The opaque `*mut Vm` (ABI param 0), passed to runtime helpers.
    vm: ClValue,
    /// The register-stack base pointer (ABI param 1) — passed unchanged to a direct callee (which
    /// computes its own frame pointer from it and its base). Valid across a direct call because that
    /// path is only taken when no `reserve_window` reallocation can occur.
    regs: ClValue,
    frame_ptr: ClValue,
    /// The frame's base offset (ABI param 2), passed to the call helper.
    base: ClValue,
    globals: ClValue,
    /// The opaque `*mut Vec<Frame>` / `*mut Vec<Value>` (ABI params 4/5), passed to the call helper.
    frames: ClValue,
    regs_vec: ClValue,
    /// Whether this prototype carries heap values in registers (it makes a call): register writes then
    /// release the overwritten value and moved heap values are retained. See [`Codegen::store_reg`].
    heap_aware: bool,
    /// Per-op may-hold-heap map, `heap_in[pc * nreg + r]` (P-JIT bare stores): whether register `r` may
    /// hold a heap pointer at the *start* of op `pc`. A `false` cell is a guarantee the register holds an
    /// immediate there, so the refcount work keyed on it — releasing an overwritten value, releasing a
    /// dropped value, retaining a moved value — is a no-op and can be skipped (a bare store/move/drop).
    /// All-false for a non-`heap_aware` prototype (already bare); all-true for a `heap_aware` prototype
    /// the analysis cannot model. See [`heap_in_map`] and [`Codegen::may_be_heap`].
    heap_in: Vec<bool>,
    /// Register count per frame — the stride into [`Codegen::heap_in`].
    nreg: usize,
    /// Ownership-transfer peephole map, indexed by bytecode pc (P-JIT bare stores). A `Move dst <- src`
    /// immediately followed by `Drop src` is an ownership *transfer*: the interpreter retains `src` in
    /// the `Move` then releases it in the `Drop`, a pair that cancels exactly regardless of the value's
    /// type. Both pcs are marked here so the `Move` skips its retain and the `Drop` skips its release —
    /// a bare copy — while the net refcount is preserved. See [`transfer_pairs`].
    transfer: Vec<bool>,
    /// The bytecode pc of the op currently being emitted, so [`Codegen::may_be_heap`] can index
    /// [`Codegen::heap_in`]. Set at the top of [`emit_op`].
    cur_pc: usize,
    /// P-JSSA: the register plan — per-pc liveness (the bail spill set) and per-pc SSA-residency
    /// permission (`ssa_ok`, the complement of [`Codegen::heap_in`]). See [`plan::RegPlan`].
    plan: plan::RegPlan,
    /// P-JSSA: one Cranelift SSA variable per VM register. In a modeled prototype the variable
    /// holds the truth for **every** register — heap values included (S5) — and the slot may be
    /// stale (stale-immediate, or heap-desynced per [`slot_hazard_map`]); in an unmodeled
    /// prototype the slot holds the truth. The
    /// frontend's variable machinery turns defs/uses into block parameters at merges — including
    /// loop headers — so a promoted loop-carried value never touches memory inside the loop.
    /// Registers the plan never promotes still get a (never-used) variable so indexing is direct.
    vars: Vec<cranelift_frontend::Variable>,
    /// T1 typed promotion: one **raw** SSA variable per VM register, holding the *unboxed* value
    /// (sign-extended i64 for `Int`, 0/1 for `Bool`) wherever the kind analysis claims that kind
    /// ([`Codegen::kinds`]). The invariant: at every pc where `kind_in ∈ {Int, Bool}` for a
    /// promoted register, its raw variable is current — every def form that can produce those
    /// kinds (`box_int_and_store`, the comparison arms, `LoadConst`, `Move`, the entry inits)
    /// also defines the raw form. Where the kind is `Imm`/`Float` the raw variable is stale and
    /// never read. Spills always go through the boxed form ([`Codegen::vars`], kept current by
    /// the same dual defs).
    raw_vars: Vec<cranelift_frontend::Variable>,
    /// T1: the per-pc kind map ([`kind_in_map`]), `kinds[pc * nreg + r]`. Consulted through
    /// [`Codegen::kind_claim`], which gates on promotion/residency.
    kinds: Vec<Kind>,
    /// P-JSSA: registers holding one statically-known immediate constant (LICM's hoisted
    /// constants). Reads inline the constant (feeding Cranelift's constant folding — an operand
    /// tag check against a known constant folds away); no variable, no entry load, no block
    /// param, no spill. The slot is written once at the def and stays current. See
    /// [`plan::const_reg_bits`].
    const_bits: Vec<Option<u64>>,
    /// This prototype's index, passed to the call helper so it can read the `Op::Call` back.
    proto: u32,
    /// P-JSSA S4.1: is this the **fast-convention** body? Entered only fresh at pc 0 with its
    /// window uninitialized and its arguments as machine arguments; every native exit runs
    /// [`Codegen::normalize_frame`] instead of a plain spill, returns two words
    /// (outcome, value), and `Op::Return` tears the frame down natively.
    fast: bool,
    /// Fast bodies only: the must-slot-written map ([`must_slot_written_map`]) driving
    /// `normalize_frame`'s keep/unit-fill decision. Empty for normal bodies.
    must_written: Vec<bool>,
    /// S5: the per-pc slot-hazard map ([`slot_hazard_map`]) — where a slot may be heap-desynced
    /// from its variable, so every sync point must spill it (live or not).
    slot_hazard: Vec<bool>,
    /// Byte offset of a `Vec`'s length word within its three-word header ([`FrameLayout`]),
    /// baked so the fast return can pop the frame and truncate the register stack natively.
    vec_len_off: i32,
    /// The full frame/`Vec` layout (S4.2): the native frame push needs the data-pointer and
    /// capacity words and the `Frame` field offsets, not just the length word.
    layout: FrameLayout,
    /// Per-pc inline-cache slot addresses (S4.2), nonzero exactly at `Call`/`CallGlobal` pcs.
    site_addrs: Vec<u64>,
    /// The empty-`Frame` template's address (S4.2) — the native push copies it, then patches
    /// `proto`/`base`/`ret_dst`.
    frame_template_addr: u64,
    note_bound_ref: FuncRef,
    retain_ref: FuncRef,
    release_ref: FuncRef,
    release_value_ref: FuncRef,
    call_ref: FuncRef,
    return_ref: FuncRef,
    prepare_call_ref: FuncRef,
    after_call_ref: FuncRef,
    leaf_op_ref: FuncRef,
    callee_sig: cranelift_codegen::ir::SigRef,
    /// Imported fast-convention signatures, one per distinct call arity in this chunk (P-JSSA
    /// S4.1) — a compiled caller `call_indirect`s a fast body through the signature its own
    /// static argument count determines.
    fast_sigs: std::collections::HashMap<usize, cranelift_codegen::ir::SigRef>,
}

impl Codegen<'_, '_> {
    /// Load register `r` (a full NaN-boxed word) from the frame window — the raw slot, bypassing
    /// SSA residency. Op emitters read through [`Codegen::read_reg`]; this is for the sites where
    /// the slot is the point (the entry parameter guard, spills, variable initialization).
    fn load_reg(&mut self, r: Reg) -> ClValue {
        self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.frame_ptr,
            reg_offset(r),
        )
    }

    /// Whether register `r` lives in an SSA variable (anywhere): promotable by the plan and not a
    /// known constant (a constant inlines at each read instead — cheaper than occupying a
    /// register through the loop).
    fn is_var(&self, r: Reg) -> bool {
        self.plan.promotable(r) && self.const_bits[r as usize].is_none()
    }

    /// Read register `r`'s current value: the inlined constant for a known-constant register,
    /// the SSA variable for a promoted one (S5: residency is universal in a modeled prototype —
    /// heap values included), else the slot (unmodeled prototypes only).
    fn read_reg(&mut self, r: Reg) -> ClValue {
        if let Some(bits) = self.const_bits[r as usize] {
            return self.b.ins().iconst(types::I64, bits as i64);
        }
        if self.is_var(r) {
            self.b.use_var(self.vars[r as usize])
        } else {
            self.load_reg(r)
        }
    }

    /// Whether register `r` may hold a heap pointer at the current op (`cur_pc`) — the bare-store
    /// analysis ([`heap_in_map`]). `false` is a guarantee it holds an immediate, so any refcount work
    /// keyed on `r`'s current value (releasing it, retaining it) is a no-op and can be skipped.
    fn may_be_heap(&self, r: Reg) -> bool {
        self.heap_in[self.cur_pc * self.nreg + r as usize]
    }

    /// Store `v` into register `r`. Reproduces the interpreter's `set_reg`, which drops one
    /// reference to the overwritten value — but only *where that value may be a heap pointer*
    /// ([`Codegen::may_be_heap`] of `r`). The caller is responsible for retaining `v` when it is
    /// a moved heap value (`LoadGlobal`/`Move`).
    ///
    /// P-JSSA S5: for a promoted register the def is a pure `def_var` — the old value is
    /// released **from the variable** (no load-old) and the slot is not written at all. The slot
    /// may then be heap-desynced (holding a released pointer, or missing the reference the
    /// variable now owns); the [`slot_hazard_map`] tracks exactly that, and every sync point
    /// spills `live ∪ hazard` before anything can read or release the slot. Only an unmodeled
    /// prototype's (unpromoted) registers take the classic slot path.
    fn store_reg(&mut self, r: Reg, v: ClValue) {
        if self.is_var(r) {
            if self.may_be_heap(r) {
                let old = self.b.use_var(self.vars[r as usize]);
                self.release_if_heap(old);
            }
            self.b.def_var(self.vars[r as usize], v);
            return;
        }
        if self.may_be_heap(r) {
            let old = self.load_reg(r);
            self.b
                .ins()
                .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
            self.release_if_heap(old);
        } else {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
        }
    }

    /// A store to register `r` with no release of the old value (the caller has already taken
    /// ownership of the old value, e.g. `Drop`, or is initializing). Same S5 residency rule as
    /// [`Codegen::store_reg`], minus the release.
    fn store_reg_raw(&mut self, r: Reg, v: ClValue) {
        if self.is_var(r) {
            self.b.def_var(self.vars[r as usize], v);
            return;
        }
        self.b
            .ins()
            .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
    }

    /// P-JSSA: materialize the SSA-resident registers into their slots for a **bail** or helper
    /// sync at `pc`: every promoted register that is **live** (the interpreter may read it) or
    /// **slot-hazardous** (S5 — the slot may hold a released pointer, or miss the heap reference
    /// the variable owns; teardown/unwind releases every slot, so it must be re-synced). A dead,
    /// non-hazardous register's slot stays stale-immediate or in-sync — safe either way.
    fn spill_ssa(&mut self, pc: usize) {
        for r in 0..self.nreg as u16 {
            if self.is_var(r)
                && (self.plan.live_at(pc, r) || self.slot_hazard[pc * self.nreg + r as usize])
            {
                let v = self.b.use_var(self.vars[r as usize]);
                self.b
                    .ins()
                    .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
            }
        }
    }

    /// Soundness guard at a **mid-frame entry** (OSR loop header or seam resume): verify at
    /// runtime that every register the analyses make a claim about at `pc` actually satisfies it.
    /// The forward models describe tier-0's fall-through state, but a mid-frame entry begins with
    /// whatever tier 0 actually left in the slots — and tier 0 heap-boxes an overflowing
    /// arithmetic result where the models claim an immediate (or an `Int`). A false claim would
    /// skip a needed retain/release (a leak, or a double-release) or misread a pointer as a raw
    /// int. Each claimed register gets the check its claim needs — `is_small_int` for a
    /// `Kind::Int` claim, the bool/float word tests for `Bool`/`Float`, the plain `is_pointer`
    /// test for an untyped immediate claim; the typed tests subsume the pointer test. The cost
    /// lands only on interpreter transitions: a native→native direct call never re-enters through
    /// the seam (its callee provably ran fully native, so the caller's claims still hold), and an
    /// OSR entry fires once per hot loop, never per iteration. A violation bails back to the
    /// interpreter at `pc`. Known-constant registers need no guard: their single dominating
    /// `LoadConst` def wrote the slot.
    fn guard_entry_claims(&mut self, pc: usize) {
        let l = Value::NANBOX;
        let mut any: Option<ClValue> = None;
        for r in 0..self.nreg as u16 {
            if self.heap_in[pc * self.nreg + r as usize] || self.const_bits[r as usize].is_some() {
                continue;
            }
            let v = self.load_reg(r);
            let viol = match self.kind_claim(pc, r) {
                Kind::Int => {
                    // Violated unless the small-int tag matches.
                    let mask = self
                        .b
                        .ins()
                        .iconst(types::I64, (l.sign_bit | l.qnan | l.int_tag) as i64);
                    let want = self.b.ins().iconst(types::I64, (l.qnan | l.int_tag) as i64);
                    let masked = self.b.ins().band(v, mask);
                    self.b.ins().icmp(IntCC::NotEqual, masked, want)
                }
                Kind::Bool => {
                    // Violated unless the word is exactly `true` or `false`.
                    let tb = self.b.ins().iconst(types::I64, l.true_bits as i64);
                    let fb = self.b.ins().iconst(types::I64, l.false_bits as i64);
                    let is_t = self.b.ins().icmp(IntCC::Equal, v, tb);
                    let is_f = self.b.ins().icmp(IntCC::Equal, v, fb);
                    let is_bool = self.b.ins().bor(is_t, is_f);
                    self.b.ins().bxor_imm(is_bool, 1)
                }
                Kind::Float => {
                    // Violated if the word is qnan-tagged (every non-f64 value is).
                    let qnan = self.b.ins().iconst(types::I64, l.qnan as i64);
                    let masked = self.b.ins().band(v, qnan);
                    self.b.ins().icmp(IntCC::Equal, masked, qnan)
                }
                Kind::Imm | Kind::Bot => self.is_pointer(v),
            };
            any = Some(match any {
                None => viol,
                Some(acc) => self.b.ins().bor(acc, viol),
            });
        }
        if let Some(any) = any {
            let ok = self.b.create_block();
            let bail = self.b.create_block();
            self.b.ins().brif(any, bail, &[], ok, &[]);
            self.b.switch_to_block(bail);
            let here = self.pc_const(pc);
            self.b.ins().return_(&[here]);
            self.b.switch_to_block(ok);
        }
    }

    /// The statically-claimed kind of register `r` at `pc` (T1): the exact kind of a
    /// known-constant register, the kind map's claim for a promoted one (a typed claim implies
    /// the immediate claim — the map is `Imm` wherever the value may be heap), else `Imm`.
    /// `Bot` (an analysis-unreached cell) degrades to `Imm`.
    fn kind_claim(&self, pc: usize, r: Reg) -> Kind {
        if let Some(bits) = self.const_bits[r as usize] {
            return classify_immediate_bits(bits);
        }
        if self.is_var(r) {
            match self.kinds[pc * self.nreg + r as usize] {
                Kind::Bot => Kind::Imm,
                k => k,
            }
        } else {
            Kind::Imm
        }
    }

    /// Read register `r`'s **raw** (unboxed i64) value — only valid where
    /// [`Codegen::kind_claim`] is `Int`: the inlined unboxed constant for a known-constant
    /// register, else the raw variable.
    fn read_raw_int(&mut self, r: Reg) -> ClValue {
        if let Some(bits) = self.const_bits[r as usize] {
            let raw = ((bits & Value::NANBOX.ptr_mask) as i64) << 16 >> 16;
            return self.b.ins().iconst(types::I64, raw);
        }
        self.b.use_var(self.raw_vars[r as usize])
    }

    /// Read register `r`'s raw bool (0/1 in an i64) — only valid where the claim is `Bool`.
    fn read_raw_bool(&mut self, r: Reg) -> ClValue {
        if let Some(bits) = self.const_bits[r as usize] {
            let raw = (bits == Value::NANBOX.true_bits) as i64;
            return self.b.ins().iconst(types::I64, raw);
        }
        self.b.use_var(self.raw_vars[r as usize])
    }

    /// Define register `r`'s raw variable (a no-op for an unpromoted register). Callers do this
    /// at every def whose result kind can be `Int`/`Bool` — see [`Codegen::raw_vars`].
    fn def_raw(&mut self, r: Reg, raw: ClValue) {
        if self.is_var(r) {
            self.b.def_var(self.raw_vars[r as usize], raw);
        }
    }

    /// T1: define every promoted register's raw variable at a native entry point — the real
    /// unboxed value where the (just-guarded) claim is `Int`/`Bool`, a dummy zero elsewhere (the
    /// frontend needs a def on every path; a dummy is never read, because a raw read requires a
    /// typed claim and a typed claim requires typed defs on every incoming path). Runs after
    /// [`Codegen::load_ssa_vars`], so the boxed variables hold the slot values.
    fn init_raw_vars(&mut self, pc: usize) {
        let l = Value::NANBOX;
        for r in 0..self.nreg as u16 {
            if !self.is_var(r) {
                continue;
            }
            let raw = match self.kind_claim(pc, r) {
                Kind::Int => {
                    let v = self.b.use_var(self.vars[r as usize]);
                    self.unbox_int(v)
                }
                Kind::Bool => {
                    let v = self.b.use_var(self.vars[r as usize]);
                    let tb = self.b.ins().iconst(types::I64, l.true_bits as i64);
                    let is_t = self.b.ins().icmp(IntCC::Equal, v, tb);
                    self.b.ins().uextend(types::I64, is_t)
                }
                _ => self.b.ins().iconst(types::I64, 0),
            };
            self.b.def_var(self.raw_vars[r as usize], raw);
        }
    }

    /// Mode-aware pre-exit sync: a normal body materializes the SSA-resident live set
    /// ([`Codegen::spill_ssa`] — the rest of its window is already tier-0-valid); a fast body
    /// must make the *whole* window valid ([`Codegen::normalize_frame`] — its window started as
    /// garbage). Called before every bail, every frame-visible helper, and every call.
    fn sync_frame(&mut self, pc: usize) {
        if self.fast {
            self.normalize_frame(pc);
        } else {
            self.spill_ssa(pc);
        }
    }

    /// P-JSSA S4.1: make a fast body's (initially uninitialized) window fully tier-0-valid at
    /// `pc`, slot by slot. A promoted register spills its variable when it is **live** (the
    /// interpreter may read it), **slot-hazardous** (S5 — the slot misses a release or a
    /// reference), or **may-heap** (S5 — the variable may own a heap reference that must reach
    /// the slot for teardown to release; unit-filling it would leak). A must-written slot (a
    /// known constant's) already holds its real value — kept. Everything else gets `unit`:
    /// exactly tier-0's never-written local, or a dead immediate whose slot may not keep garbage
    /// bits (teardown's release loop and the unwind path read every slot).
    fn normalize_frame(&mut self, pc: usize) {
        let unit = self
            .b
            .ins()
            .iconst(types::I64, Value::NANBOX.unit_bits as i64);
        for r in 0..self.nreg as u16 {
            let i = pc * self.nreg + r as usize;
            if self.is_var(r)
                && (self.plan.live_at(pc, r) || self.slot_hazard[i] || self.heap_in[i])
            {
                let v = self.b.use_var(self.vars[r as usize]);
                self.b
                    .ins()
                    .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
            } else if !self.must_written[i] {
                self.b
                    .ins()
                    .store(MemFlagsData::trusted(), unit, self.frame_ptr, reg_offset(r));
            }
        }
    }

    /// Mode-aware bail return: a normal body returns one word (the resume pc / outcome); a fast
    /// body's signature returns two (outcome, value) — the value is meaningful only for
    /// `OUTCOME_RETURNED`, so bails pad with zero.
    fn ret_bail(&mut self, outcome: ClValue) {
        if self.fast {
            let zero = self.b.ins().iconst(types::I64, 0);
            self.b.ins().return_(&[outcome, zero]);
        } else {
            self.b.ins().return_(&[outcome]);
        }
    }

    /// P-JSSA: (re)load every promotable register's variable from its slot. Used at every native
    /// entry point (fresh pc-0 entry, resume-after-call, OSR header) — where the interpreter's
    /// slots are the truth — and after a runtime helper that may have written a slot (a call's
    /// return value, a leaf op's destination). Sound after a helper because [`Codegen::spill_ssa`]
    /// ran first: a live register's slot was just made current, a helper-written slot is fresh,
    /// and a dead register's (possibly stale) slot value is never read before its next def.
    fn load_ssa_vars(&mut self) {
        for r in 0..self.nreg as u16 {
            if self.is_var(r) {
                let v = self.load_reg(r);
                self.b.def_var(self.vars[r as usize], v);
            }
        }
    }

    /// Emit `if is_pointer(v) { retain(v) }` — bump the refcount of a moved heap value.
    fn retain_if_heap(&mut self, v: ClValue) {
        let heap = self.is_pointer(v);
        let do_it = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(heap, do_it, &[], cont, &[]);
        self.b.switch_to_block(do_it);
        let f = self.retain_ref;
        self.b.ins().call(f, &[v]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// Emit `if is_pointer(v) { release(v) }` — drop one reference to an overwritten heap value
    /// (matching the interpreter's `set_reg`, which uses the plain, non-destructor release).
    fn release_if_heap(&mut self, v: ClValue) {
        let heap = self.is_pointer(v);
        let do_it = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(heap, do_it, &[], cont, &[]);
        self.b.switch_to_block(do_it);
        let f = self.release_ref;
        self.b.ins().call(f, &[v]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// Emit the heap release for a dropped value (`Op::Drop`): the destructor-aware `release_value`
    /// (which may run a `destruct` block if this is the last reference) when the drop is IR-relevant,
    /// else the plain `release` — matching the interpreter's `Drop` arm.
    fn release_dropped_if_heap(&mut self, v: ClValue, relevant: bool) {
        if !relevant {
            self.release_if_heap(v);
            return;
        }
        let heap = self.is_pointer(v);
        let do_it = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(heap, do_it, &[], cont, &[]);
        self.b.switch_to_block(do_it);
        let f = self.release_value_ref;
        let vm = self.vm;
        self.b.ins().call(f, &[vm, v]);
        self.b.ins().jump(cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// `(v & (sign|qnan)) == (sign|qnan)` — is `v` a heap pointer?
    fn is_pointer(&mut self, v: ClValue) -> ClValue {
        let l = Value::NANBOX;
        let mask = self
            .b
            .ins()
            .iconst(types::I64, (l.sign_bit | l.qnan) as i64);
        let masked = self.b.ins().band(v, mask);
        self.b.ins().icmp(IntCC::Equal, masked, mask)
    }

    /// `(v & (sign|qnan|int_tag)) == (qnan|int_tag)` — is `v` an immediate small int?
    fn is_small_int(&mut self, v: ClValue) -> ClValue {
        let l = Value::NANBOX;
        let mask = self
            .b
            .ins()
            .iconst(types::I64, (l.sign_bit | l.qnan | l.int_tag) as i64);
        let want = self.b.ins().iconst(types::I64, (l.qnan | l.int_tag) as i64);
        let masked = self.b.ins().band(v, mask);
        self.b.ins().icmp(IntCC::Equal, masked, want)
    }

    /// Unbox a small-int word to its i64: sign-extend the low 48-bit payload (`(p << 16) >> 16`).
    fn unbox_int(&mut self, v: ClValue) -> ClValue {
        let l = Value::NANBOX;
        let pm = self.b.ins().iconst(types::I64, l.ptr_mask as i64);
        let p = self.b.ins().band(v, pm);
        let shl = self.b.ins().ishl_imm(p, 16);
        self.b.ins().sshr_imm(shl, 16)
    }

    /// `(v & qnan) != qnan` — is `v` an f64 float? (Every tagged value — int/bool/unit/f32/pointer —
    /// has all the qnan bits set; a float is exactly the words that don't.)
    fn is_float(&mut self, v: ClValue) -> ClValue {
        let qnan = self.b.ins().iconst(types::I64, Value::NANBOX.qnan as i64);
        let masked = self.b.ins().band(v, qnan);
        self.b.ins().icmp(IntCC::NotEqual, masked, qnan)
    }

    /// Reinterpret a float word's bits as the f64 it stores (the value is known to be a float — a
    /// float is stored as its own bit pattern, not tagged).
    fn bits_to_f64(&mut self, v: ClValue) -> ClValue {
        self.b.ins().bitcast(types::F64, MemFlagsData::new(), v)
    }

    /// `v == unbound` — is `v` the VM's unbound-global sentinel?
    fn is_unbound(&mut self, v: ClValue) -> ClValue {
        let u = self
            .b
            .ins()
            .iconst(types::I64, Value::NANBOX.unbound_bits as i64);
        self.b.ins().icmp(IntCC::Equal, v, u)
    }

    /// Load global slot `g` (a full NaN-boxed word) from the globals array.
    fn load_global(&mut self, g: u32) -> ClValue {
        self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.globals,
            global_offset(g),
        )
    }

    /// Store `v` into global slot `g`.
    fn store_global(&mut self, g: u32, v: ClValue) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), v, self.globals, global_offset(g));
    }

    /// A native i64 outcome constant — a resume pc (bail) for the compiled-fn return.
    fn pc_const(&mut self, pc: usize) -> ClValue {
        self.b.ins().iconst(types::I64, pc as i64)
    }
}

/// Register `r`'s byte offset within the frame window (`r * sizeof(Value)`).
fn reg_offset(r: Reg) -> i32 {
    (r as i32) * 8
}

/// Global slot `g`'s byte offset within the globals array (`g * sizeof(Value)`).
fn global_offset(g: u32) -> i32 {
    (g as i32) * 8
}

/// Emit the native code for one op into its (already switched-to) block. `op_blocks[pc]` maps a
/// bytecode pc to its Cranelift block, for jumps/branches; a bail returns the pc. An op the JIT does
/// not compile ([`is_fast_op`] is false) bails here — the interpreter runs it and the rest of the
/// frame.
fn emit_op(cg: &mut Codegen, consts: &[Const], op: &Op, pc: usize, op_blocks: &[Block]) {
    cg.cur_pc = pc;
    if !is_fast_op(op, consts) {
        cg.sync_frame(pc);
        let here = cg.pc_const(pc);
        cg.ret_bail(here);
        return;
    }
    let next = |cg: &mut Codegen| cg.b.ins().jump(op_blocks[pc + 1], &[]);
    match op {
        Op::LoadConst { dst, k } => {
            let c = &consts[*k as usize];
            let bits = const_immediate_bits(c).expect("eligibility checked");
            let v = cg.b.ins().iconst(types::I64, bits as i64);
            // T1: a typed constant also defines the raw form (the kind transfer gives the
            // destination this constant's kind).
            match c {
                Const::Int(i) => {
                    let raw = cg.b.ins().iconst(types::I64, *i);
                    cg.def_raw(*dst, raw);
                }
                Const::Bool(bl) => {
                    let raw = cg.b.ins().iconst(types::I64, *bl as i64);
                    cg.def_raw(*dst, raw);
                }
                _ => {}
            }
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Move { dst, src } => {
            // The interpreter's `Move` retains the source then overwrites the destination. The retain is
            // skipped when this `Move` is the head of an ownership transfer (a `Drop src` follows, whose
            // release cancels it) or where `src` is provably an immediate (the bare-store analysis);
            // otherwise it retains the moved heap value. `store_reg` releases the overwritten destination
            // where it may be heap.
            let v = cg.read_reg(*src);
            if !cg.transfer[pc] && cg.may_be_heap(*src) {
                cg.retain_if_heap(v);
            }
            // T1: the destination inherits the source's kind, so its raw form moves too.
            match cg.kind_claim(pc, *src) {
                Kind::Int => {
                    let raw = cg.read_raw_int(*src);
                    cg.def_raw(*dst, raw);
                }
                Kind::Bool => {
                    let raw = cg.read_raw_bool(*src);
                    cg.def_raw(*dst, raw);
                }
                _ => {}
            }
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Drop { reg, relevant } => {
            // Take the value out (leaving `unit`) and drop it. The release is skipped when this `Drop` is
            // the tail of an ownership transfer (the preceding `Move` took its retain, which this release
            // would cancel) or where the dropped value is provably an immediate (the bare-store
            // analysis); otherwise a heap value is released — the destructor-aware path when the drop is
            // IR-relevant, else the plain release.
            let v = cg.read_reg(*reg);
            let unit =
                cg.b.ins()
                    .iconst(types::I64, Value::NANBOX.unit_bits as i64);
            cg.store_reg_raw(*reg, unit);
            if !cg.transfer[pc] && cg.may_be_heap(*reg) {
                cg.release_dropped_if_heap(v, *relevant);
            }
            next(cg);
        }
        Op::Binary { op, dst, a, b, .. } => emit_binary(cg, *op, *dst, *a, *b, pc, op_blocks),
        Op::Jump { target } => {
            cg.b.ins().jump(op_blocks[*target as usize], &[]);
        }
        Op::JumpIfTrue { reg, target } => {
            // T1: a `Bool`-claimed scrutinee branches on its raw 0/1 form — no re-comparison.
            if cg.kind_claim(pc, *reg) == Kind::Bool {
                let raw = cg.read_raw_bool(*reg);
                cg.b.ins().brif(
                    raw,
                    op_blocks[*target as usize],
                    &[],
                    op_blocks[pc + 1],
                    &[],
                );
                return;
            }
            // Taken iff the value is exactly `true`; a non-bool is simply not taken (the interpreter's
            // `as_bool() == Some(true)`), so no guard/bail is needed.
            let v = cg.read_reg(*reg);
            let t =
                cg.b.ins()
                    .iconst(types::I64, Value::NANBOX.true_bits as i64);
            let is_true = cg.b.ins().icmp(IntCC::Equal, v, t);
            cg.b.ins().brif(
                is_true,
                op_blocks[*target as usize],
                &[],
                op_blocks[pc + 1],
                &[],
            );
        }
        Op::JumpIfFalse { reg, target } => {
            if cg.kind_claim(pc, *reg) == Kind::Bool {
                let raw = cg.read_raw_bool(*reg);
                cg.b.ins().brif(
                    raw,
                    op_blocks[pc + 1],
                    &[],
                    op_blocks[*target as usize],
                    &[],
                );
                return;
            }
            let v = cg.read_reg(*reg);
            let fb =
                cg.b.ins()
                    .iconst(types::I64, Value::NANBOX.false_bits as i64);
            let is_false = cg.b.ins().icmp(IntCC::Equal, v, fb);
            cg.b.ins().brif(
                is_false,
                op_blocks[*target as usize],
                &[],
                op_blocks[pc + 1],
                &[],
            );
        }
        Op::CondBranch { reg, target, .. } => {
            // T1: a `Bool`-claimed condition needs no E0007 bail chain — the claim (entry-guarded
            // / comparison-defined) proves the word is a bool, so branch on the raw bit.
            if cg.kind_claim(pc, *reg) == Kind::Bool {
                let raw = cg.read_raw_bool(*reg);
                cg.b.ins().brif(
                    raw,
                    op_blocks[pc + 1],
                    &[],
                    op_blocks[*target as usize],
                    &[],
                );
                return;
            }
            // false → jump target; true → fall through; anything else → bail so the interpreter
            // raises E0007 ("`if` condition must be a bool").
            let v = cg.read_reg(*reg);
            let l = Value::NANBOX;
            let fb = cg.b.ins().iconst(types::I64, l.false_bits as i64);
            let tb = cg.b.ins().iconst(types::I64, l.true_bits as i64);
            let is_false = cg.b.ins().icmp(IntCC::Equal, v, fb);
            let chk_true = cg.b.create_block();
            cg.b.ins()
                .brif(is_false, op_blocks[*target as usize], &[], chk_true, &[]);
            cg.b.switch_to_block(chk_true);
            let is_true = cg.b.ins().icmp(IntCC::Equal, v, tb);
            let bail = cg.b.create_block();
            cg.b.ins().brif(is_true, op_blocks[pc + 1], &[], bail, &[]);
            cg.b.switch_to_block(bail);
            cg.sync_frame(pc);
            let here = cg.pc_const(pc);
            cg.ret_bail(here);
        }
        Op::LoadGlobal { dst, global, .. } => emit_load_global(cg, *dst, global.0, pc, op_blocks),
        Op::StoreGlobal { global, src } => emit_store_global(cg, global.0, *src, pc, op_blocks),
        Op::TakeGlobal { dst, global, .. } => emit_take_global(cg, *dst, global.0, pc, op_blocks),
        Op::Call {
            dst, callee, args, ..
        } => emit_call(cg, *dst, args, CalleeSrc::Reg(*callee), pc, op_blocks),
        Op::CallGlobal {
            dst, global, args, ..
        } => emit_call(cg, *dst, args, CalleeSrc::Global(global.0), pc, op_blocks),
        Op::Return { src } => emit_return(cg, *src, pc),
        op if is_leaf_heap_op(op) => emit_leaf_op(cg, pc, op_blocks),
        // A bail point (`is_fast_op` was checked at the top; unreachable in practice).
        _ => {
            let here = cg.pc_const(pc);
            cg.ret_bail(here);
        }
    }
}

/// A native `Call` (P-JIT J3): hand the whole call to the `jit_call` runtime helper, which reads the
/// `Op::Call` back from `proto`/`pc` and runs the shared closure-call setup on the interpreter's
/// frame/register stacks (pushing the callee frame). Whatever it returns — `CALLED` (frame pushed),
/// a resume pc (a synchronous first-class-builtin call completed), or `ABORTED` — becomes this
/// compiled function's outcome, so the interpreter runs the callee and resumes the caller in tier 0.
/// The source of a call's callee value: a register (`Op::Call`) or a global slot
/// (`Op::CallGlobal`) — the S4.2 inline cache loads it natively for the hit check.
enum CalleeSrc {
    Reg(Reg),
    Global(u32),
}

fn emit_call(
    cg: &mut Codegen,
    dst: Reg,
    args: &[Reg],
    callee_src: CalleeSrc,
    pc: usize,
    op_blocks: &[Block],
) {
    // P-JSSA sync point: the call helpers read the argument registers (and, on a bail or an
    // abort-unwind, any slot) from the window, so make it fully valid first (mode-aware).
    cg.sync_frame(pc);
    let vm = cg.vm;
    let frames = cg.frames;
    let regs_vec = cg.regs_vec;
    let base = cg.base;
    let l = cg.layout;
    let ptr_off = (l.vec_ptr_word * 8) as i32;
    let cap_off = (l.vec_cap_word * 8) as i32;
    let len_off = cg.vec_len_off;

    // The shared fast-call block (S4.1/S4.2), parameterized on (untagged fast entry, callee
    // window base) so both the inline-cache hit and the helper's tagged result enter it.
    let fast_blk = cg.b.create_block();
    cg.b.append_block_param(fast_blk, types::I64);
    cg.b.append_block_param(fast_blk, types::I64);
    let slow_blk = cg.b.create_block();

    // ---- S4.2 inline-cache hit path: no helper at all. The cached key is the pinned callee
    // closure's exact bits; a hit proves the same live closure — same prototype, same fast
    // entry. The frame push is emitted natively: capacity-check both stacks (miss → the slow
    // helper, which can grow them), extend the register stack's length over the (uninitialized —
    // S4.1's contract) callee window, copy the empty-`Frame` template, patch
    // `proto`/`base`/`ret_dst`, bump the frame count, and record the caller's resume pc.
    let site_addr = cg.site_addrs[pc];
    let site = cg.b.ins().iconst(types::I64, site_addr as i64);
    let callee_v = match callee_src {
        CalleeSrc::Reg(r) => cg.read_reg(r),
        CalleeSrc::Global(g) => cg.load_global(g),
    };
    let key =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), site, 0);
    let is_hit = cg.b.ins().icmp(IntCC::Equal, callee_v, key);
    let hit_blk = cg.b.create_block();
    cg.b.ins().brif(is_hit, hit_blk, &[], slow_blk, &[]);

    cg.b.switch_to_block(hit_blk);
    let regs_len =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), regs_vec, len_off);
    let regs_cap =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), regs_vec, cap_off);
    let nregs =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), site, 16);
    let need = cg.b.ins().iadd(regs_len, nregs);
    let fits =
        cg.b.ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, need, regs_cap);
    let frames_len =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), frames, len_off);
    let frames_cap =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), frames, cap_off);
    let froom =
        cg.b.ins()
            .icmp(IntCC::UnsignedLessThan, frames_len, frames_cap);
    let room = cg.b.ins().band(fits, froom);
    let push_blk = cg.b.create_block();
    cg.b.ins().brif(room, push_blk, &[], slow_blk, &[]);

    cg.b.switch_to_block(push_blk);
    // regs.set_len(len + nregs) — the callee window, uninitialized by contract.
    cg.b.ins()
        .store(MemFlagsData::trusted(), need, regs_vec, len_off);
    let fdata =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), frames, ptr_off);
    let foff = cg.b.ins().imul_imm(frames_len, l.frame_size as i64);
    let faddr = cg.b.ins().iadd(fdata, foff);
    // frames.push(template) — write the empty frame's (emission-time-constant) words, then
    // patch the per-call fields. The template is fully built before compilation, so its words
    // are baked as immediates: no loads on the hot path.
    // SAFETY: `frame_template_addr` is the VM-owned template `Frame`'s address (alive for the
    // `Vm`'s — and thus this `Jit`'s — lifetime), word-aligned and a multiple-of-8 size
    // (`Jit::new` checked).
    let template_words: Vec<u64> = unsafe {
        std::slice::from_raw_parts(cg.frame_template_addr as *const u64, l.frame_size / 8).to_vec()
    };
    for (w, &word) in template_words.iter().enumerate() {
        let c = cg.b.ins().iconst(types::I64, word as i64);
        cg.b.ins()
            .store(MemFlagsData::trusted(), c, faddr, (w * 8) as i32);
    }
    let proto32 =
        cg.b.ins()
            .load(types::I32, MemFlagsData::trusted(), site, 24);
    cg.b.ins().store(
        MemFlagsData::trusted(),
        proto32,
        faddr,
        l.proto_offset as i32,
    );
    cg.b.ins().store(
        MemFlagsData::trusted(),
        regs_len,
        faddr,
        l.base_offset as i32,
    );
    let dst16 = cg.b.ins().iconst(types::I16, dst as i64);
    cg.b.ins().store(
        MemFlagsData::trusted(),
        dst16,
        faddr,
        l.ret_dst_offset as i32,
    );
    let new_flen = cg.b.ins().iadd_imm(frames_len, 1);
    cg.b.ins()
        .store(MemFlagsData::trusted(), new_flen, frames, len_off);
    // The caller (the current top frame, just below the pushed one) resumes at pc + 1 if the
    // callee deopts — same eager update the helper performs.
    let caller_faddr = cg.b.ins().iadd_imm(faddr, -(l.frame_size as i64));
    let resume = cg.b.ins().iconst(types::I64, pc as i64 + 1);
    cg.b.ins().store(
        MemFlagsData::trusted(),
        resume,
        caller_faddr,
        l.pc_offset as i32,
    );
    let cached_fp =
        cg.b.ins()
            .load(types::I64, MemFlagsData::trusted(), site, 8);
    cg.b.ins()
        .jump(fast_blk, &[cached_fp.into(), regs_len.into()]);

    // ---- Slow path: the prepare helper (which also fills or poisons the site cache). ----
    cg.b.switch_to_block(slow_blk);
    let proto = cg.b.ins().iconst(types::I32, cg.proto as i64);
    let pcv = cg.b.ins().iconst(types::I32, pc as i64);

    // Try a direct native→native call: `prepare_call` returns the callee's compiled entry pointer
    // and its reserved window base in one roundtrip (S4.0), or a zero pointer if the call is not
    // direct-able (uncompiled callee, defaults/upvalues, no stack capacity). A pointer with bit 0
    // set is a **fast-convention** entry (S4.1): its window was reserved uninitialized and the
    // arguments travel as machine arguments.
    let prep = cg.prepare_call_ref;
    let pinst =
        cg.b.ins()
            .call(prep, &[vm, frames, regs_vec, base, proto, pcv, site]);
    let fnptr = cg.b.inst_results(pinst)[0];
    let callee_base = cg.b.inst_results(pinst)[1];
    let fast_bit = cg.b.ins().band_imm(fnptr, 1);
    let tagged_blk = cg.b.create_block();
    let untagged = cg.b.create_block();
    cg.b.ins().brif(fast_bit, tagged_blk, &[], untagged, &[]);
    cg.b.switch_to_block(tagged_blk);
    let fp_untagged = cg.b.ins().band_imm(fnptr, -2);
    cg.b.ins()
        .jump(fast_blk, &[fp_untagged.into(), callee_base.into()]);

    cg.b.switch_to_block(untagged);
    let zero = cg.b.ins().iconst(types::I64, 0);
    let is_zero = cg.b.ins().icmp(IntCC::Equal, fnptr, zero);
    let fallback = cg.b.create_block();
    let direct = cg.b.create_block();
    cg.b.ins().brif(is_zero, fallback, &[], direct, &[]);

    // Fallback: the shared `jit_call` path (pushes a frame, returns CALLED/resume-pc/ABORTED).
    cg.b.switch_to_block(fallback);
    let call = cg.call_ref;
    let inst =
        cg.b.ins()
            .call(call, &[vm, frames, regs_vec, base, proto, pcv]);
    let outcome = cg.b.inst_results(inst)[0];
    cg.ret_bail(outcome);

    // Direct: call the callee's compiled entry on the shared stack. `prepare_call` already reserved
    // the callee window (whose base it returned) and pushed its frame; `regs`/`globals`/`frames`/
    // `regs_vec` pass through and `entry_pc = 0` (a fresh frame). No reallocation can happen
    // (capacity was checked), so `cg.regs` stays valid across the indirect call.
    cg.b.switch_to_block(direct);
    let regs = cg.regs;
    let globals = cg.globals;
    let entry0 = cg.b.ins().iconst(types::I64, 0);
    let iinst = cg.b.ins().call_indirect(
        cg.callee_sig,
        fnptr,
        &[vm, regs, callee_base, globals, frames, regs_vec, entry0],
    );
    let callee_outcome = cg.b.inst_results(iinst)[0];
    let continue_blk = cg.b.create_block();
    let return_blk = cg.b.create_block();
    // Hot path inlined (P-CALL S2): a completed direct call returns `OUTCOME_RETURNED` (its result is
    // already in `dst` via the value-returning `Return`), which `after_call` would only map to
    // `CONTINUE` — so branch straight to the next op, skipping that per-call helper call. Only a
    // non-RETURNED outcome (a bail pc that must fix the callee frame's pc, or a nested CALLED/ABORTED)
    // takes the cold `after_call` path.
    let returned = cg.b.ins().iconst(types::I64, OUTCOME_RETURNED);
    let is_returned = cg.b.ins().icmp(IntCC::Equal, callee_outcome, returned);
    let cold_blk = cg.b.create_block();
    cg.b.ins()
        .brif(is_returned, continue_blk, &[], cold_blk, &[]);
    cg.b.switch_to_block(cold_blk);
    let ainst =
        cg.b.ins()
            .call(cg.after_call_ref, &[vm, frames, callee_outcome]);
    let after = cg.b.inst_results(ainst)[0];
    let cont = cg.b.ins().iconst(types::I64, OUTCOME_CONTINUE);
    let is_cont = cg.b.ins().icmp(IntCC::Equal, after, cont);
    cg.b.ins().brif(is_cont, continue_blk, &[], return_blk, &[]);
    cg.b.switch_to_block(continue_blk);
    // S4.1a precise reload: this block is reached only by a clean direct `RETURNED`, whose whole
    // effect on the caller's window is one write — the destination slot (`do_return`'s transfer).
    // Every other promoted variable still holds exactly what it held at the pre-call spill, so
    // only `dst`'s variable needs a reload. (Its raw variable needs nothing: a call result's kind
    // is `Imm`, and the raw form already has a def on every path from the entry inits.)
    if cg.is_var(dst) {
        let v = cg.load_reg(dst);
        cg.b.def_var(cg.vars[dst as usize], v);
    }
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
    cg.b.switch_to_block(return_blk);
    cg.ret_bail(after);

    // Fast-convention direct call (S4.1): pass the argument *values* as machine arguments — no
    // window fill, no argument copy/retain happened — and receive the result as the second
    // return word. `OUTCOME_RETURNED` → write the result into `dst` ourselves (the ownership
    // transfer `do_return` would have done) and continue; anything else takes the same cold
    // `after_call` protocol as the classic direct path (the callee frame exists — it normalized
    // its window before exiting). Entered from the inline-cache hit (S4.2) or the helper's
    // tagged result, via the block params (untagged entry, callee base).
    cg.b.switch_to_block(fast_blk);
    let fp = cg.b.block_params(fast_blk)[0];
    let f_callee_base = cg.b.block_params(fast_blk)[1];
    let mut fast_args: Vec<ClValue> =
        vec![vm, cg.regs, f_callee_base, cg.globals, frames, regs_vec];
    for &a in args {
        let v = cg.read_reg(a);
        fast_args.push(v);
    }
    let fsig = cg.fast_sigs[&args.len()];
    let finst = cg.b.ins().call_indirect(fsig, fp, &fast_args);
    let f_outcome = cg.b.inst_results(finst)[0];
    let f_value = cg.b.inst_results(finst)[1];
    let f_returned = cg.b.ins().iconst(types::I64, OUTCOME_RETURNED);
    let f_is_ret = cg.b.ins().icmp(IntCC::Equal, f_outcome, f_returned);
    let f_hot = cg.b.create_block();
    let f_cold = cg.b.create_block();
    cg.b.ins().brif(f_is_ret, f_hot, &[], f_cold, &[]);
    cg.b.switch_to_block(f_hot);
    // The value arrives with the reference the callee's teardown retained for it; `store_reg`
    // releases the overwritten destination and takes ownership — exactly `do_return`'s transfer.
    cg.store_reg(dst, f_value);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
    cg.b.switch_to_block(f_cold);
    let f_ainst = cg.b.ins().call(cg.after_call_ref, &[vm, frames, f_outcome]);
    let f_after = cg.b.inst_results(f_ainst)[0];
    let f_cont = cg.b.ins().iconst(types::I64, OUTCOME_CONTINUE);
    let f_is_cont = cg.b.ins().icmp(IntCC::Equal, f_after, f_cont);
    let f_ret_blk = cg.b.create_block();
    cg.b.ins()
        .brif(f_is_cont, continue_blk, &[], f_ret_blk, &[]);
    cg.b.switch_to_block(f_ret_blk);
    cg.ret_bail(f_after);
}

/// A native leaf heap/collection op (P-JIT J4): run it through the `run_leaf_op` helper, which does
/// the interpreter's exact logic (refcounts included) and returns `OUTCOME_CONTINUE` (done — continue
/// to `pc + 1`) or a resume pc (it bailed — a dispatch or an error the interpreter must handle).
fn emit_leaf_op(cg: &mut Codegen, pc: usize, op_blocks: &[Block]) {
    // P-JSSA sync point: the leaf helper reads its operands from (and writes its destination to)
    // the slots.
    cg.sync_frame(pc);
    let vm = cg.vm;
    let regs_vec = cg.regs_vec;
    let base = cg.base;
    let proto = cg.b.ins().iconst(types::I32, cg.proto as i64);
    let pcv = cg.b.ins().iconst(types::I32, pc as i64);
    let inst =
        cg.b.ins()
            .call(cg.leaf_op_ref, &[vm, regs_vec, base, proto, pcv]);
    let outcome = cg.b.inst_results(inst)[0];
    let cont = cg.b.ins().iconst(types::I64, OUTCOME_CONTINUE);
    let is_cont = cg.b.ins().icmp(IntCC::Equal, outcome, cont);
    let continue_blk = cg.b.create_block();
    let return_blk = cg.b.create_block();
    cg.b.ins().brif(is_cont, continue_blk, &[], return_blk, &[]);
    cg.b.switch_to_block(continue_blk);
    // The helper wrote the op's destination slot — reload the SSA variables before continuing.
    cg.load_ssa_vars();
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
    cg.b.switch_to_block(return_blk);
    cg.ret_bail(outcome);
}

/// A native `Op::Return` (P-JIT J3): hand the return value to the `jit_return` helper, which runs the
/// shared return protocol (transfer to the caller's destination, pop this frame) on the interpreter's
/// stacks, and propagate its outcome (`RETURNED`, or `HALTED` for the bottom frame). Value-returning
/// so a native direct caller gets its callee's result back without a bail.
///
/// S4.0 fast teardown: the helper also gets a **release mask** — the bare-store analysis's
/// may-heap row at this return pc (for ≤64 registers; `u64::MAX` = release-all beyond that) — so
/// the window teardown releases only the slots that can actually hold a heap value. Sound because
/// this code path executes only natively, where the analysis's claims are maintained (entries
/// verify them, native defs preserve them); a clear bit's release would be a no-op on the
/// immediate the slot holds.
fn emit_return(cg: &mut Codegen, src: Reg, pc: usize) {
    if cg.fast {
        return emit_return_fast(cg, src, pc);
    }
    // The frame dies here; only the may-heap slots need to be current — the helper's masked
    // teardown releases exactly those (S4.0), and with S5 their truth lives in the variables, so
    // spill the masked set first (an unmasked slot is a stale immediate or dangling bits the
    // teardown never touches). The return value travels as the helper's argument.
    let raw = cg.read_reg(src);
    let mask: u64 = if cg.nreg <= 64 {
        let row = &cg.heap_in[pc * cg.nreg..pc * cg.nreg + cg.nreg];
        row.iter()
            .enumerate()
            .fold(0u64, |m, (r, &h)| if h { m | (1 << r) } else { m })
    } else {
        u64::MAX
    };
    if mask != u64::MAX {
        for r in 0..cg.nreg as u16 {
            if mask & (1 << r) != 0 && cg.is_var(r) {
                let v = cg.b.use_var(cg.vars[r as usize]);
                cg.b.ins()
                    .store(MemFlagsData::trusted(), v, cg.frame_ptr, reg_offset(r));
            }
        }
    } else {
        // Release-all fallback (>64 registers): every slot the helper will release must be
        // current.
        cg.spill_ssa(pc);
    }
    let vm = cg.vm;
    let frames = cg.frames;
    let regs_vec = cg.regs_vec;
    let f = cg.return_ref;
    let maskv = cg.b.ins().iconst(types::I64, mask as i64);
    let inst = cg.b.ins().call(f, &[vm, frames, regs_vec, raw, maskv]);
    let outcome = cg.b.inst_results(inst)[0];
    cg.b.ins().return_(&[outcome]);
}

/// A fast body's `Op::Return` (P-JSSA S4.1): the whole return protocol, natively — no helper.
/// The value goes back as the second return word; the frame it leaves behind was pushed by
/// `jit_prepare_call`'s fast path (empty upvalues, `RetTransform::None`, never the bottom frame),
/// so the teardown is: retain the value if it may be heap (it must survive the window release),
/// release exactly the may-heap slots (each is must-written — `fast_ok` — so it holds a real
/// value; everything else is an immediate or was never written), pop the frame by decrementing
/// the frame `Vec`'s length (nothing to drop: empty `Vec` + POD fields), and truncate the
/// register stack to this frame's base by storing it as the register `Vec`'s length.
fn emit_return_fast(cg: &mut Codegen, src: Reg, pc: usize) {
    let raw = cg.read_reg(src);
    if cg.may_be_heap(src) {
        cg.retain_if_heap(raw);
    }
    // S5: the may-heap registers' truth lives in their variables (the slots may be desynced or
    // never written) — release from the variables directly, no loads.
    for r in 0..cg.nreg as u16 {
        if cg.heap_in[pc * cg.nreg + r as usize] {
            let v = if cg.is_var(r) {
                cg.b.use_var(cg.vars[r as usize])
            } else {
                cg.load_reg(r)
            };
            cg.release_if_heap(v);
        }
    }
    // frames.len -= 1 (pop this frame).
    let flen = cg.b.ins().load(
        types::I64,
        MemFlagsData::trusted(),
        cg.frames,
        cg.vec_len_off,
    );
    let flen1 = cg.b.ins().iadd_imm(flen, -1);
    cg.b.ins()
        .store(MemFlagsData::trusted(), flen1, cg.frames, cg.vec_len_off);
    // regs.len = base (truncate this frame's window off the register stack).
    let base = cg.base;
    cg.b.ins()
        .store(MemFlagsData::trusted(), base, cg.regs_vec, cg.vec_len_off);
    let outcome = cg.b.ins().iconst(types::I64, OUTCOME_RETURNED);
    cg.b.ins().return_(&[outcome, raw]);
}

/// `dst = globals[g]` (P-JIT globals). Bails if the slot is unbound (E0005). A call-free prototype
/// also bails on a heap global (it would need a `retain`, breaking the immediate invariant); a
/// `heap_aware` prototype instead retains the heap value (matching the interpreter's `LoadGlobal`
/// retain) and `store_reg` releases the overwritten destination.
fn emit_load_global(cg: &mut Codegen, dst: Reg, g: u32, pc: usize, op_blocks: &[Block]) {
    let v = cg.load_global(g);
    let unbound = cg.is_unbound(v);
    let bail_cond = if cg.heap_aware {
        unbound
    } else {
        let heap = cg.is_pointer(v);
        cg.b.ins().bor(unbound, heap)
    };
    let cont = cg.b.create_block();
    let bail = cg.b.create_block();
    cg.b.ins().brif(bail_cond, bail, &[], cont, &[]);
    cg.b.switch_to_block(bail);
    cg.sync_frame(pc);
    let here = cg.pc_const(pc);
    cg.ret_bail(here);
    cg.b.switch_to_block(cont);
    if cg.heap_aware {
        cg.retain_if_heap(v);
    }
    cg.store_reg(dst, v);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// `globals[g] = take(reg[src])` (P-JIT globals; `StoreGlobal` moves the source out, leaving `unit`).
/// `src` is an immediate (invariant), so the global takes it with no retain. The old occupant decides
/// the path: unbound → write it and call the helper to record the first binding in `global_order`; a
/// heap value → bail (its `release` may run a destructor); an immediate → plain overwrite (its release
/// is a no-op).
fn emit_store_global(cg: &mut Codegen, g: u32, src: Reg, pc: usize, op_blocks: &[Block]) {
    // Decide the bail BEFORE mutating anything: a bail hands control back to the interpreter, which
    // re-runs this op, so no register or slot may have changed yet. The only bail here is a bound heap
    // old value (its `release` may run a destructor) — `is_pointer` is false for the unbound sentinel,
    // so it excludes the first-bind case.
    let old = cg.load_global(g);
    let heap = cg.is_pointer(old);
    let cont = cg.b.create_block();
    let bail = cg.b.create_block();
    cg.b.ins().brif(heap, bail, &[], cont, &[]);
    cg.b.switch_to_block(bail);
    cg.sync_frame(pc);
    let here = cg.pc_const(pc);
    cg.ret_bail(here);

    // Not a heap old value: safe to mutate. Take the source out (moved into the global — no release,
    // its reference transfers) and write the slot; a first bind also records it in `global_order`.
    cg.b.switch_to_block(cont);
    let v = cg.read_reg(src);
    let unit =
        cg.b.ins()
            .iconst(types::I64, Value::NANBOX.unit_bits as i64);
    cg.store_reg_raw(src, unit);
    cg.store_global(g, v);
    let is_unb = cg.is_unbound(old);
    let bind_blk = cg.b.create_block();
    let after = cg.b.create_block();
    cg.b.ins().brif(is_unb, bind_blk, &[], after, &[]);
    cg.b.switch_to_block(bind_blk);
    let vm = cg.vm;
    let gid = cg.b.ins().iconst(types::I32, g as i64);
    let note = cg.note_bound_ref;
    cg.b.ins().call(note, &[vm, gid]);
    cg.b.ins().jump(after, &[]);
    cg.b.switch_to_block(after);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// `dst = take(globals[g])` — move the global out, leaving `unit` bound (P-JIT globals). Bails if
/// unbound (E0005) or heap (moving a heap value into `dst` with no retain would break the immediate
/// invariant — the interpreter does it). An immediate transfers with no refcount.
fn emit_take_global(cg: &mut Codegen, dst: Reg, g: u32, pc: usize, op_blocks: &[Block]) {
    let old = cg.load_global(g);
    let unbound = cg.is_unbound(old);
    let heap = cg.is_pointer(old);
    let bail_cond = cg.b.ins().bor(unbound, heap);
    let cont = cg.b.create_block();
    let bail = cg.b.create_block();
    cg.b.ins().brif(bail_cond, bail, &[], cont, &[]);
    cg.b.switch_to_block(bail);
    cg.sync_frame(pc);
    let here = cg.pc_const(pc);
    cg.ret_bail(here);
    cg.b.switch_to_block(cont);
    let unit =
        cg.b.ins()
            .iconst(types::I64, Value::NANBOX.unit_bits as i64);
    cg.store_global(g, unit);
    cg.store_reg(dst, old);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// Emit a numeric `Binary`. T1 typed promotion first consults the static kind claims
/// ([`Codegen::kind_claim`]): both operands claimed `Int` → the raw integer body directly, no
/// tag checks, no unboxing (the operands come from the raw variables); both claimed `Float` →
/// the float body directly. Otherwise fall back to the runtime dispatch (the bytecode is
/// untyped): both small ints → the integer fast path (J1); both f64 floats → the float fast path
/// (J2); anything else (mixed int/float, f32, objects, …) → bail to the interpreter, which
/// handles the widening/coercion and any type error.
fn emit_binary(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    a: Reg,
    b: Reg,
    pc: usize,
    op_blocks: &[Block],
) {
    let ka = cg.kind_claim(pc, a);
    let kb = cg.kind_claim(pc, b);
    if ka == Kind::Int && kb == Kind::Int {
        let x = cg.read_raw_int(a);
        let y = cg.read_raw_int(b);
        emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);
        return;
    }
    if ka == Kind::Float && kb == Kind::Float {
        let va = cg.read_reg(a);
        let vb = cg.read_reg(b);
        emit_float_binary(cg, op, dst, va, vb, pc, op_blocks);
        return;
    }
    // Asymmetric typed paths (T1b): exactly one side statically `Int` (resp. `Float`), the other
    // an unknown immediate — guard only the unknown side, then the typed body. The canonical
    // case is a loop bounded by a parameter (`i < n`): `i` is claimed `Int`, `n` is a promoted
    // `Imm` whose boxed variable is loop-invariant, so its one guard hoists out of the loop.
    // (A statically-known mismatch — `Int` × `Float`, `Int` × `Bool` — keeps the generic
    // dispatch, which bails to the interpreter exactly as before.)
    if (ka == Kind::Int && kb == Kind::Imm) || (ka == Kind::Imm && kb == Kind::Int) {
        let (x, y) = if ka == Kind::Int {
            let x = cg.read_raw_int(a);
            let vb = cg.read_reg(b);
            let ok = cg.is_small_int(vb);
            let cont = cg.b.create_block();
            guard(cg, ok, cont, pc);
            (x, cg.unbox_int(vb))
        } else {
            let va = cg.read_reg(a);
            let ok = cg.is_small_int(va);
            let cont = cg.b.create_block();
            guard(cg, ok, cont, pc);
            let y = cg.read_raw_int(b);
            (cg.unbox_int(va), y)
        };
        emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);
        return;
    }
    if (ka == Kind::Float && kb == Kind::Imm) || (ka == Kind::Imm && kb == Kind::Float) {
        let va = cg.read_reg(a);
        let vb = cg.read_reg(b);
        let unknown = if ka == Kind::Float { vb } else { va };
        let ok = cg.is_float(unknown);
        let cont = cg.b.create_block();
        guard(cg, ok, cont, pc);
        emit_float_binary(cg, op, dst, va, vb, pc, op_blocks);
        return;
    }

    let va = cg.read_reg(a);
    let vb = cg.read_reg(b);

    let a_int = cg.is_small_int(va);
    let b_int = cg.is_small_int(vb);
    let both_int = cg.b.ins().band(a_int, b_int);

    let int_block = cg.b.create_block();
    let float_check = cg.b.create_block();
    cg.b.ins().brif(both_int, int_block, &[], float_check, &[]);

    // Integer fast path.
    cg.b.switch_to_block(int_block);
    let x = cg.unbox_int(va);
    let y = cg.unbox_int(vb);
    emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);

    // Float fast path (or bail): both operands must be f64 floats.
    cg.b.switch_to_block(float_check);
    let a_flt = cg.is_float(va);
    let b_flt = cg.is_float(vb);
    let both_flt = cg.b.ins().band(a_flt, b_flt);
    let float_block = cg.b.create_block();
    guard(cg, both_flt, float_block, pc);
    emit_float_binary(cg, op, dst, va, vb, pc, op_blocks);
}

/// The integer body of a `Binary`, entered with both operands as **raw** (unboxed, sign-extended)
/// i64s — either unboxed by the runtime dispatch or read straight from the raw variables (T1).
/// Computes in i64 with the interpreter's wrapping/trapping semantics and stores the boxed result
/// — bailing before any write on a zero divisor or an out-of-immediate-range result.
fn emit_int_binary_raw(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    x: ClValue,
    y: ClValue,
    pc: usize,
    op_blocks: &[Block],
) {
    match op {
        BinaryOp::Add => {
            let r = cg.b.ins().iadd(x, y);
            box_int_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Sub => {
            let r = cg.b.ins().isub(x, y);
            box_int_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Mul => {
            let r = cg.b.ins().imul(x, y);
            box_int_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Div | BinaryOp::Rem => {
            // Bail on a zero divisor (the interpreter raises E0008). Signed overflow (MIN / -1) cannot
            // arise: `x` is unboxed from a 48-bit immediate, so it is never i64::MIN.
            let zero = cg.b.ins().iconst(types::I64, 0);
            let nonzero = cg.b.ins().icmp(IntCC::NotEqual, y, zero);
            let ok = cg.b.create_block();
            guard(cg, nonzero, ok, pc);
            let r = if op == BinaryOp::Div {
                cg.b.ins().sdiv(x, y)
            } else {
                cg.b.ins().srem(x, y)
            };
            box_int_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let cc = match op {
                BinaryOp::Eq => IntCC::Equal,
                BinaryOp::Ne => IntCC::NotEqual,
                BinaryOp::Lt => IntCC::SignedLessThan,
                BinaryOp::Le => IntCC::SignedLessThanOrEqual,
                BinaryOp::Gt => IntCC::SignedGreaterThan,
                _ => IntCC::SignedGreaterThanOrEqual,
            };
            let cmp = cg.b.ins().icmp(cc, x, y);
            // Select the exact `true`/`false` NaN-box bits from the i1 comparison result, and
            // keep the raw (0/1) form current for a `Bool`-claimed destination (T1) — a claimed
            // `CondBranch` then branches on the raw bit with no re-comparison.
            let raw = cg.b.ins().uextend(types::I64, cmp);
            cg.def_raw(dst, raw);
            let l = Value::NANBOX;
            let tb = cg.b.ins().iconst(types::I64, l.true_bits as i64);
            let fb = cg.b.ins().iconst(types::I64, l.false_bits as i64);
            let boxed = cg.b.ins().select(cmp, tb, fb);
            cg.store_reg(dst, boxed);
            cg.b.ins().jump(op_blocks[pc + 1], &[]);
        }
        _ => unreachable!("supported_binary gate: unexpected op {op:?}"),
    }
}

/// Box an i64 arithmetic result back to a small-int word and store it, or bail if it overflows the
/// 48-bit immediate range (the interpreter would heap-box it). The fit test: sign-extending the low
/// 48 bits must reproduce the value. Also keeps the raw form current (T1): a stored result *is*
/// the raw i64 (it just passed the fit check), so an `Int`-claimed downstream read skips the
/// unboxing entirely.
fn box_int_and_store(cg: &mut Codegen, dst: Reg, r: ClValue, pc: usize, op_blocks: &[Block]) {
    let l = Value::NANBOX;
    let pm = cg.b.ins().iconst(types::I64, l.ptr_mask as i64);
    let lo = cg.b.ins().band(r, pm);
    let shl = cg.b.ins().ishl_imm(lo, 16);
    let ext = cg.b.ins().sshr_imm(shl, 16);
    let fits = cg.b.ins().icmp(IntCC::Equal, ext, r);

    // Guard: result fits the 48-bit immediate range, else bail (a big int must heap-box).
    let store = cg.b.create_block();
    guard(cg, fits, store, pc);
    cg.def_raw(dst, r);
    let tag = cg.b.ins().iconst(types::I64, (l.qnan | l.int_tag) as i64);
    let boxed = cg.b.ins().bor(lo, tag);
    cg.store_reg(dst, boxed);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// The float body of a `Binary`, entered with both operands proven f64 floats (J2). Computes in f64
/// and stores the boxed result. Matches the interpreter's `arithmetic`/`compare`: ordered
/// comparisons (false on NaN, except `!=` which is true on NaN), and a NaN arithmetic result
/// canonicalized to the standard quiet NaN — exactly `Value::float`. `%` has no Cranelift instruction
/// (`fmod` is a libcall), so it bails.
fn emit_float_binary(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    va: ClValue,
    vb: ClValue,
    pc: usize,
    op_blocks: &[Block],
) {
    // Float `%` (fmod) is a libcall, not an instruction — leave it to the interpreter.
    if op == BinaryOp::Rem {
        let here = cg.pc_const(pc);
        cg.b.ins().return_(&[here]);
        return;
    }
    let x = cg.bits_to_f64(va);
    let y = cg.bits_to_f64(vb);
    match op {
        BinaryOp::Add => {
            let r = cg.b.ins().fadd(x, y);
            box_float_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Sub => {
            let r = cg.b.ins().fsub(x, y);
            box_float_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Mul => {
            let r = cg.b.ins().fmul(x, y);
            box_float_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Div => {
            let r = cg.b.ins().fdiv(x, y);
            box_float_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let cc = match op {
                BinaryOp::Eq => FloatCC::Equal,
                BinaryOp::Ne => FloatCC::NotEqual,
                BinaryOp::Lt => FloatCC::LessThan,
                BinaryOp::Le => FloatCC::LessThanOrEqual,
                BinaryOp::Gt => FloatCC::GreaterThan,
                _ => FloatCC::GreaterThanOrEqual,
            };
            let cmp = cg.b.ins().fcmp(cc, x, y);
            // Keep the raw bool form current for a `Bool`-claimed destination (T1) — the kind
            // transfer marks every comparison result `Bool` regardless of which path produced it.
            let raw = cg.b.ins().uextend(types::I64, cmp);
            cg.def_raw(dst, raw);
            let l = Value::NANBOX;
            let tb = cg.b.ins().iconst(types::I64, l.true_bits as i64);
            let fb = cg.b.ins().iconst(types::I64, l.false_bits as i64);
            let boxed = cg.b.ins().select(cmp, tb, fb);
            cg.store_reg(dst, boxed);
            cg.b.ins().jump(op_blocks[pc + 1], &[]);
        }
        _ => unreachable!("supported_binary gate: unexpected op {op:?}"),
    }
}

/// Box an f64 arithmetic result to a float word and store it, matching `Value::float`: a NaN result
/// is canonicalized to the standard quiet NaN (so it can never collide with the tag space), any other
/// value keeps its own bit pattern.
fn box_float_and_store(cg: &mut Codegen, dst: Reg, r: ClValue, pc: usize, op_blocks: &[Block]) {
    let raw = cg.b.ins().bitcast(types::I64, MemFlagsData::new(), r);
    let is_nan = cg.b.ins().fcmp(FloatCC::Unordered, r, r);
    let canon =
        cg.b.ins()
            .iconst(types::I64, Value::float(f64::NAN).bits() as i64);
    let boxed = cg.b.ins().select(is_nan, canon, raw);
    cg.store_reg(dst, boxed);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// Emit a fast-path guard: `brif cond -> cont else bail(pc)`, fill the bail block (which spills
/// the SSA-resident live registers, then hands control back to the interpreter at `pc`), and
/// leave the builder positioned in `cont` so the caller keeps emitting the fast path. `cont` is a
/// caller-created block; `cond` is the keep-going condition (true = stay in native code). No
/// sealing here — blocks are sealed once at the end (`seal_all_blocks`), which also resolves the
/// SSA variables' block parameters (P-JSSA).
fn guard(cg: &mut Codegen, cond: ClValue, cont: Block, pc: usize) {
    let bail = cg.b.create_block();
    cg.b.ins().brif(cond, cont, &[], bail, &[]);
    cg.b.switch_to_block(bail);
    cg.sync_frame(pc);
    let here = cg.pc_const(pc);
    cg.ret_bail(here);
    cg.b.switch_to_block(cont);
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_bytecode::Chunk;
    use noeta_span::Span;

    fn chunk(code: Vec<Op>, consts: Vec<Const>, num_params: u16, num_registers: u16) -> Chunk {
        let mut c = Chunk::placeholder();
        c.code = code;
        c.consts = consts;
        c.num_params = num_params;
        c.num_registers = num_registers;
        c
    }

    /// The ownership-transfer peephole marks a `Move dst <- src` immediately followed by `Drop src`.
    #[test]
    fn transfer_pairs_marks_move_then_drop_of_source() {
        let c = chunk(
            vec![
                Op::Move { dst: 0, src: 1 },
                Op::Drop {
                    reg: 1,
                    relevant: false,
                },
                Op::Halt,
            ],
            vec![],
            0,
            2,
        );
        assert_eq!(transfer_pairs(&c), vec![true, true, false]);
    }

    /// A `Drop` of a *different* register than the preceding `Move`'s source is not a transfer.
    #[test]
    fn transfer_pairs_ignores_drop_of_other_register() {
        let c = chunk(
            vec![
                Op::Move { dst: 0, src: 1 },
                Op::Drop {
                    reg: 2,
                    relevant: false,
                },
                Op::Halt,
            ],
            vec![],
            0,
            3,
        );
        assert_eq!(transfer_pairs(&c), vec![false, false, false]);
    }

    /// A `Drop` that is itself a branch target is reachable without the `Move`, so the retain/release
    /// pairing cannot be assumed — it is left alone.
    #[test]
    fn transfer_pairs_ignores_drop_that_is_a_jump_target() {
        let c = chunk(
            vec![
                Op::Jump { target: 2 },
                Op::Move { dst: 0, src: 1 },
                Op::Drop {
                    reg: 1,
                    relevant: false,
                }, // pc 2 — a jump target
                Op::Halt,
            ],
            vec![],
            0,
            2,
        );
        let t = transfer_pairs(&c);
        assert!(
            !t[1] && !t[2],
            "a jump-targeted Drop is not a safe transfer"
        );
    }

    /// The may-hold-heap analysis: parameters are **claimed** immediate at entry (T1b — the pc-0
    /// guard bails on a heap argument, and every mid-frame entry verifies the claim), a
    /// comparison result is an immediate, and so is a completed arithmetic result — native
    /// `Binary` bails to the interpreter *before* storing when the result would overflow the
    /// 48-bit immediate range. A call's destination, by contrast, stays may-heap.
    #[test]
    fn heap_in_marks_call_results_but_not_params_or_binary_results() {
        let sp = Span::new(0, 0);
        let c = chunk(
            vec![
                Op::Binary {
                    op: BinaryOp::Lt,
                    dst: 1,
                    a: 0,
                    b: 0,
                    span: sp,
                },
                Op::Binary {
                    op: BinaryOp::Add,
                    dst: 2,
                    a: 0,
                    b: 0,
                    span: sp,
                },
                Op::Call {
                    dst: 1,
                    callee: 2,
                    args: Box::new([]),
                    span: sp,
                },
                Op::Halt,
            ],
            vec![],
            1, // r0 is a parameter
            3,
        );
        let map = heap_in_map(&c, true);
        let nreg = 3;
        let at = |pc: usize, r: usize| map[pc * nreg + r];
        assert!(
            !at(0, 0),
            "a parameter is claimed immediate at entry (guard-verified)"
        );
        assert!(!at(0, 1), "a fresh temp is an immediate at entry");
        assert!(!at(1, 1), "a comparison result is an immediate");
        assert!(
            !at(2, 2),
            "a natively-stored arithmetic result is an immediate (overflow bails before the store)"
        );
        assert!(at(3, 1), "a call result stays may-heap");
    }

    /// A non-`heap_aware` prototype already stores bare everywhere — the map is all-false.
    #[test]
    fn heap_in_all_false_when_not_heap_aware() {
        let c = chunk(vec![Op::Halt], vec![], 1, 2);
        assert!(heap_in_map(&c, false).iter().all(|b| !*b));
    }

    /// The kind fixpoint (T1): a `LoadConst` int def is `Int` and survives a loop's back edge (the
    /// header join of the pre-loop path and the in-loop redef is `Int ∨ Int = Int`); a parameter
    /// is `Imm` (unknown caller value); a comparison result is `Bool`; arithmetic on
    /// statically-`Int` operands is `Int`.
    #[test]
    fn kind_map_tracks_ints_and_bools_through_a_loop() {
        let sp = Span::new(0, 0);
        // Shape of a counting loop: 0: r1 = 0   1: r2 = r1 < r0   2: cond r2 → 5
        //                           3: r1 = r1 + r1   4: jump 1   5: halt
        let c = chunk(
            vec![
                Op::LoadConst { dst: 1, k: 0 },
                Op::Binary {
                    op: BinaryOp::Lt,
                    dst: 2,
                    a: 1,
                    b: 0,
                    span: sp,
                },
                Op::CondBranch {
                    reg: 2,
                    target: 5,
                    span: sp,
                },
                Op::Binary {
                    op: BinaryOp::Add,
                    dst: 1,
                    a: 1,
                    b: 1,
                    span: sp,
                },
                Op::Jump { target: 1 },
                Op::Halt,
            ],
            vec![Const::Int(0)],
            1, // r0 is a parameter
            3,
        );
        let k = kind_in_map(&c);
        let nreg = 3;
        let at = |pc: usize, r: usize| k[pc * nreg + r];
        assert_eq!(at(1, 0), Kind::Imm, "a parameter's kind is unknown");
        assert_eq!(at(1, 1), Kind::Int, "the counter is Int at the header join");
        assert_eq!(at(2, 2), Kind::Bool, "a comparison result is Bool");
        assert_eq!(at(4, 1), Kind::Int, "Int + Int stays Int around the loop");
    }

    /// A typed kind claim never outlives the immediate claim it rides on: wherever the kind map
    /// says `Int`/`Bool`/`Float`, the bare-store map must say not-heap — both transfers mark the
    /// same may-heap defs. (The loop chunk above exercises params, consts, arith, and joins.)
    #[test]
    fn typed_kind_implies_immediate_claim() {
        let sp = Span::new(0, 0);
        let c = chunk(
            vec![
                Op::LoadConst { dst: 1, k: 0 },
                Op::Binary {
                    op: BinaryOp::Lt,
                    dst: 2,
                    a: 1,
                    b: 0,
                    span: sp,
                },
                Op::Binary {
                    op: BinaryOp::Add,
                    dst: 1,
                    a: 1,
                    b: 0,
                    span: sp,
                },
                Op::Move { dst: 2, src: 1 },
                Op::Jump { target: 1 },
                Op::Halt,
            ],
            vec![Const::Int(0)],
            1,
            3,
        );
        let kinds = kind_in_map(&c);
        let heap = heap_in_map(&c, true);
        for (i, k) in kinds.iter().enumerate() {
            if matches!(k, Kind::Int | Kind::Bool | Kind::Float) {
                assert!(
                    !heap[i],
                    "typed claim at flat index {i} must imply the immediate claim"
                );
            }
        }
    }

    /// An unmodeled op fails the kind map closed to all-`Imm` — the generic (tag-checked) code.
    #[test]
    fn kind_map_all_imm_when_an_op_is_unmodeled() {
        let c = chunk(
            vec![
                Op::MakeTuple {
                    dst: 0,
                    items: Box::new([]),
                },
                Op::Halt,
            ],
            vec![],
            0,
            1,
        );
        assert!(kind_in_map(&c).iter().all(|k| *k == Kind::Imm));
    }

    /// An unmodeled op (here `MakeTuple`) opts the whole prototype out — the analysis fails closed to
    /// all-true (every store keeps its refcount-correct release).
    #[test]
    fn heap_in_all_true_when_an_op_is_unmodeled() {
        let c = chunk(
            vec![
                Op::MakeTuple {
                    dst: 0,
                    items: Box::new([]),
                },
                Op::Halt,
            ],
            vec![],
            0,
            1,
        );
        assert!(heap_in_map(&c, true).iter().all(|b| *b));
    }
}
