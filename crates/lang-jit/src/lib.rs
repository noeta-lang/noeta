//! The `lang` method JIT (milestone P-JIT) — a Cranelift backend that native-compiles hot
//! prototypes so the fast path runs as machine code instead of dispatched register bytecode.
//!
//! # Where this sits
//!
//! The interpreter ([`lang_vm`](../lang_vm/index.html)) is **tier 0**: every prototype runs by
//! `match`-dispatching its [`lang_bytecode::Op`]s. This crate is **tier 1**: a per-prototype
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
//! The value encoding is [`lang_value::Value::NANBOX`] — the single source of truth, so the inlined
//! tag checks and box/unbox sequences match the interpreter bit-for-bit.
//!
//! # Gating
//!
//! The whole JIT lives behind the `jit` cargo feature on `lang-vm`/`lang-conformance`. The default
//! build, the deterministic sandbox, and the conformance differential never pull Cranelift and are
//! byte-identical without it — the same discipline that gates the real-thread isolates.

use core::ffi::c_void;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Value as ClValue, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module as _, default_libcall_names};

use lang_ast::BinaryOp;
use lang_bytecode::{Const, Module, Op, Reg};
use lang_value::Value;

/// A compiled prototype's entry point — the tier-1 ABI.
///
/// - `vm` is an opaque `*mut Vm` (the interpreter reconstitutes `&mut Vm` from it to service
///   runtime-helper callbacks); this crate never dereferences it.
/// - `regs` points at the base of the VM's shared register stack (`Vec<Value>`), and `base` is the
///   frame's window offset, so the frame's registers are `regs[base + i]` — identical addressing to
///   the interpreter (P-VMT-FRAME).
/// - `globals` points at the base of the VM's global-slot array (`Vec<Value>`, one word per
///   [`lang_bytecode::GlobalId`]); it never grows, so the pointer is stable for the whole run. Native
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
pub const OBSERVE_HELPER: &str = "lang_jit_observe";

/// The name of the "note a global's first binding" helper. Native `StoreGlobal` writes the slot
/// itself, then calls this so the VM records the slot in `global_order` for reverse-order teardown
/// destruction — the one piece of `StoreGlobal` that can't be inlined (a `Vec` push may reallocate).
pub const NOTE_GLOBAL_BOUND_HELPER: &str = "lang_jit_note_global_bound";

/// Runtime-helper names for the heap/refcount path a call-bearing prototype needs (J3). `retain`
/// bumps a value's refcount; `release` drops one (matching the interpreter's `set_reg` overwrite);
/// `release_value` is the destructor-aware drop (for `Drop`-relevant); `call` runs the shared
/// `Op::Call` setup on the interpreter's stacks.
pub const RETAIN_HELPER: &str = "lang_jit_retain";
pub const RELEASE_HELPER: &str = "lang_jit_release";
pub const RELEASE_VALUE_HELPER: &str = "lang_jit_release_value";
pub const CALL_HELPER: &str = "lang_jit_call";
/// The `Op::Return` helper (J3 native calls): runs the shared return protocol (transfer to the
/// caller, pop the frame) and returns [`OUTCOME_RETURNED`] or [`OUTCOME_HALTED`].
pub const RETURN_HELPER: &str = "lang_jit_return";
/// Direct-call helpers (J3 native→native calls). `prepare_call` checks whether the `Op::Call` at a pc
/// can be a direct native call (compiled callee, plain arity, no upvalues, stack capacity) and, if so,
/// sets up the callee frame and returns the callee's compiled entry pointer (else `0`, a fallback to
/// `call`); `callee_base` reads the reserved callee base it stashed; `after_call` inspects the
/// callee's outcome and tells the caller to continue in place ([`OUTCOME_CONTINUE`]) or propagate.
pub const PREPARE_CALL_HELPER: &str = "lang_jit_prepare_call";
pub const CALLEE_BASE_HELPER: &str = "lang_jit_callee_base";
pub const AFTER_CALL_HELPER: &str = "lang_jit_after_call";
/// The leaf-heap-op helper (J4): runs a single non-dispatching heap/collection op (the interpreter's
/// exact arm, refcounts included) and returns [`OUTCOME_CONTINUE`] (done — the caller advances) or a
/// resume pc (it can't handle this instance — a dispatch or an error — so the interpreter runs it).
pub const LEAF_OP_HELPER: &str = "lang_jit_run_leaf_op";

/// The method JIT: a Cranelift [`JITModule`] plus a per-prototype cache of finalized entry points.
///
/// The cache is indexed by prototype index (into [`lang_bytecode::Module::protos`]) — the same key
/// the interpreter dispatches on. `compiled[p]` is `Some` once prototype `p` has been JIT-compiled;
/// the interpreter consults it at frame entry.
pub struct Jit {
    /// Owns every finalized machine-code page; must outlive every [`CompiledFn`] handed out.
    module: JITModule,
    /// Finalized entry points, keyed by prototype index. `None` = not (yet) compiled → tier 0.
    compiled: Vec<Option<CompiledFn>>,
    /// How many prototypes were compiled to *real* native code (vs a bail stub) — the coverage stat.
    native_count: usize,
    /// Imported runtime helpers, declared once (see the `*_HELPER` name constants).
    observe_id: FuncId,
    note_bound_id: FuncId,
    retain_id: FuncId,
    release_id: FuncId,
    release_value_id: FuncId,
    call_id: FuncId,
    return_id: FuncId,
    prepare_call_id: FuncId,
    callee_base_id: FuncId,
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
    pub fn new(helpers: &[(&str, *const u8)]) -> Result<Jit, String> {
        let mut flags = settings::builder();
        flags
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flags.set("is_pic", "false").map_err(|e| e.to_string())?;
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
        // `lang_jit_observe(vm: ptr)` and `lang_jit_note_global_bound(vm: ptr, g: i32)`, declared once.
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
        // `return(vm, frames, regs_vec: ptr, raw: i64) -> i64`.
        let mut return_sig = module.make_signature();
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(ptr_ty));
        return_sig.params.push(AbiParam::new(types::I64));
        return_sig.returns.push(AbiParam::new(types::I64));
        let return_id = module
            .declare_function(RETURN_HELPER, Linkage::Import, &return_sig)
            .map_err(|e| e.to_string())?;
        // Direct-call helpers. `prepare_call` shares `call`'s signature (returns a fn ptr or 0);
        // `callee_base(vm) -> usize`; `after_call(vm, frames, outcome: i64) -> i64`.
        let prepare_call_id = module
            .declare_function(PREPARE_CALL_HELPER, Linkage::Import, &call_sig)
            .map_err(|e| e.to_string())?;
        let mut callee_base_sig = module.make_signature();
        callee_base_sig.params.push(AbiParam::new(ptr_ty));
        callee_base_sig.returns.push(AbiParam::new(ptr_ty));
        let callee_base_id = module
            .declare_function(CALLEE_BASE_HELPER, Linkage::Import, &callee_base_sig)
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
            native_count: 0,
            observe_id,
            note_bound_id,
            retain_id,
            release_id,
            release_value_id,
            call_id,
            return_id,
            prepare_call_id,
            callee_base_id,
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
            self.compiled
                .resize(module.protos.len().max(proto + 1), None);
        }
        if let Some(f) = self.compiled[proto] {
            return Ok(f);
        }
        let chunk = &module.protos[proto];
        let f = if is_eligible(chunk) {
            let f = self.emit_int_body(module, proto)?;
            self.native_count += 1;
            f
        } else {
            self.emit_bail_stub(proto)?
        };
        self.compiled[proto] = Some(f);
        Ok(f)
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

    /// Finalize the current `self.ctx` under `name` and return its entry point.
    fn finalize(&mut self, name: &str) -> Result<CompiledFn, String> {
        let func_id = self
            .module
            .declare_function(name, Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        self.module
            .define_function(func_id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
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

    /// Emit the bail stub for an ineligible prototype: call the `lang_jit_observe` helper (proving
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
        self.finalize(&format!("lang_jit_stub{proto}"))
    }

    /// Emit the native integer body for a J1-eligible prototype (see the module docs). One Cranelift
    /// block per bytecode `pc`; register state lives in memory (the `regs` array), so blocks carry no
    /// SSA params — only the frame base pointer, computed once in the entry block, crosses into them.
    fn emit_int_body(&mut self, module: &Module, proto: usize) -> Result<CompiledFn, String> {
        let chunk = &module.protos[proto];
        let n = chunk.code.len();
        let reachable = reachable_pcs(chunk);

        self.module.clear_context(&mut self.ctx);
        // Precompute the ABI signature (also imported for the direct-call `call_indirect`) before the
        // builder borrows `self.ctx.func`, so it doesn't also need to borrow `self`.
        let abi_sig = self.abi_signature();
        self.ctx.func.signature = abi_sig.clone();
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
            let entry_pc = b.block_params(entry)[6];
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
            let callee_base_ref = self
                .module
                .declare_func_in_func(self.callee_base_id, b.func);
            let after_call_ref = self.module.declare_func_in_func(self.after_call_id, b.func);
            let leaf_op_ref = self.module.declare_func_in_func(self.leaf_op_id, b.func);
            // The signature of a compiled prototype, imported so a direct call can `call_indirect`
            // another compiled prototype's entry point.
            let callee_sig = b.import_signature(abi_sig.clone());

            // A prototype that makes a call carries heap values (the callee closure, and results) in
            // registers, so its register writes must be refcount-correct (release the overwritten
            // value, retain a moved heap value). A call-free prototype keeps the immediate invariant
            // (J1/J2/globals) and the faster refcount-free stores.
            let heap_aware = chunk
                .code
                .iter()
                .any(|op| matches!(op, Op::Call { .. }) || is_leaf_heap_op(op));

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
                proto: proto as u32,
                note_bound_ref,
                retain_ref,
                release_ref,
                release_value_ref,
                call_ref,
                return_ref,
                prepare_call_ref,
                callee_base_ref,
                after_call_ref,
                leaf_op_ref,
                callee_sig,
            };

            // Entry-pc dispatch (J3 resume-native): jump to the block for `entry_pc`. `0` is a fresh
            // frame (run the parameter guard first); a post-call resume pc jumps straight to its block;
            // any other value has no native entry, so bail (the interpreter runs that frame). The valid
            // resume pcs are exactly `call_pc + 1` for each `Call` (the interpreter re-enters a frame
            // only at pc 0 or just after a call returns).
            let resume_targets: Vec<usize> =
                entry_pcs(chunk).into_iter().filter(|&p| p != 0).collect();
            let guarded = cg.b.create_block();
            let bad_entry = cg.b.create_block();
            // Chain: entry_pc == 0 → guarded; == resume_pc_k → op_blocks[k]; else → bad_entry.
            let is_zero = cg.b.ins().icmp_imm(IntCC::Equal, entry_pc, 0);
            let mut next = cg.b.create_block();
            cg.b.ins().brif(is_zero, guarded, &[], next, &[]);
            for (i, &rp) in resume_targets.iter().enumerate() {
                cg.b.switch_to_block(next);
                let is_rp = cg.b.ins().icmp_imm(IntCC::Equal, entry_pc, rp as i64);
                let after = if i + 1 < resume_targets.len() {
                    cg.b.create_block()
                } else {
                    bad_entry
                };
                cg.b.ins().brif(is_rp, op_blocks[rp], &[], after, &[]);
                next = after;
            }
            if resume_targets.is_empty() {
                cg.b.switch_to_block(next);
                cg.b.ins().jump(bad_entry, &[]);
            }
            // `bad_entry`: an unexpected resume pc — hand the frame back to the interpreter there.
            // (`entry_pc` is pointer-width, i.e. i64 on the 64-bit target, matching the return.)
            cg.b.switch_to_block(bad_entry);
            cg.b.ins().return_(&[entry_pc]);

            // `guarded` (fresh frame, entry_pc == 0): parameter guard, then op block 0. If any argument
            // is a heap pointer, bail to pc 0 — keeping heap arguments out of the body (the body's heap
            // values then arise only from `LoadGlobal`/calls, which the heap-aware path refcounts).
            cg.b.switch_to_block(guarded);
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
                    cg.b.ins().brif(any, bail0, &[], op_blocks[0], &[]);
                    cg.b.switch_to_block(bail0);
                    let zero = cg.b.ins().iconst(types::I64, 0);
                    cg.b.ins().return_(&[zero]);
                }
                None => {
                    cg.b.ins().jump(op_blocks[0], &[]);
                }
            }

            // One block per op. Unreachable pcs (dead code) get a trivial bail so they never touch
            // `frame_ptr` (which only dominates reachable blocks).
            for (pc, op) in chunk.code.iter().enumerate() {
                cg.b.switch_to_block(op_blocks[pc]);
                if !reachable[pc] {
                    let here = cg.b.ins().iconst(types::I64, pc as i64);
                    cg.b.ins().return_(&[here]);
                    continue;
                }
                emit_op(&mut cg, &chunk.consts, op, pc, &op_blocks);
            }

            b.seal_all_blocks();
            b.finalize();
        }
        self.finalize(&format!("lang_jit_proto{proto}"))
    }
}

/// Whether a prototype is worth compiling: it has at least one op the JIT emits natively. Ops it
/// doesn't (calls, heap ops, `Echo`, `Return`, `Halt`, …) are *bail points* — the body runs its
/// compilable ops and hands back to the interpreter at the first one it can't (per-op bail). A
/// prototype with no fast op at all gets a bail stub instead (nothing to gain).
fn is_eligible(chunk: &lang_bytecode::Chunk) -> bool {
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
        Op::Call { .. } | Op::Return { .. } => true,
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
        Const::Str(_) | Const::NativeModule(_) => None,
    }
}

/// The bytecode pcs at which the interpreter may (re-)enter this prototype's native code: pc 0 (a
/// fresh frame) plus every `call_pc + 1` (a resume after a native `Call` returned) — J3
/// resume-native. Those are the only pcs a frame's saved `pc` ever holds at a `'reload` transition.
fn entry_pcs(chunk: &lang_bytecode::Chunk) -> Vec<usize> {
    let n = chunk.code.len();
    let mut pcs = vec![0usize];
    for (pc, op) in chunk.code.iter().enumerate() {
        if matches!(op, Op::Call { .. }) && pc + 1 < n {
            pcs.push(pc + 1);
        }
    }
    pcs
}

/// Forward reachability of each bytecode pc in the *native* control-flow graph, seeded from every
/// native entry point ([`entry_pcs`]) — a fresh frame (pc 0) and every post-call resume pc. A non-fast
/// op is terminal — it bails (returns its pc), so it has no native successor — which is why this
/// follows edges only out of fast ops. Used so the codegen fills unreachable blocks (dead code, or the
/// fall-through past a bail) with a trivial bail instead of code that would reference the entry-only
/// frame/globals pointers from a non-dominated block.
fn reachable_pcs(chunk: &lang_bytecode::Chunk) -> Vec<bool> {
    let n = chunk.code.len();
    let mut seen = vec![false; n];
    let mut stack = entry_pcs(chunk);
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
            // A native `Call` exits the compiled function (returns `CALLED`, or resumes native after a
            // direct call); `Return` ends the frame. Neither has an in-frame native successor.
            Op::Call { .. } | Op::Return { .. } => {}
            _ => stack.push(pc + 1), // fast straight-line op
        }
    }
    seen
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
    /// This prototype's index, passed to the call helper so it can read the `Op::Call` back.
    proto: u32,
    note_bound_ref: FuncRef,
    retain_ref: FuncRef,
    release_ref: FuncRef,
    release_value_ref: FuncRef,
    call_ref: FuncRef,
    return_ref: FuncRef,
    prepare_call_ref: FuncRef,
    callee_base_ref: FuncRef,
    after_call_ref: FuncRef,
    leaf_op_ref: FuncRef,
    callee_sig: cranelift_codegen::ir::SigRef,
}

impl Codegen<'_, '_> {
    /// Load register `r` (a full NaN-boxed word) from the frame window.
    fn load_reg(&mut self, r: Reg) -> ClValue {
        self.b.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.frame_ptr,
            reg_offset(r),
        )
    }

    /// Store `v` into register `r`. In a call-free prototype the overwritten value is always an
    /// immediate (the interpreter's release-on-overwrite is a no-op), so this is a bare store. In a
    /// `heap_aware` prototype the old occupant may be a heap value, so it releases it first —
    /// reproducing the interpreter's `set_reg` (which drops one reference to the old value). The caller
    /// is responsible for retaining `v` when it is a moved heap value (`LoadGlobal`/`Move`).
    fn store_reg(&mut self, r: Reg, v: ClValue) {
        if self.heap_aware {
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

    /// A plain store to register `r` with no release of the old value (the caller has already taken
    /// ownership of the old value, e.g. `Drop`, or is initializing).
    fn store_reg_raw(&mut self, r: Reg, v: ClValue) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
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
    if !is_fast_op(op, consts) {
        let here = cg.pc_const(pc);
        cg.b.ins().return_(&[here]);
        return;
    }
    let next = |cg: &mut Codegen| cg.b.ins().jump(op_blocks[pc + 1], &[]);
    match op {
        Op::LoadConst { dst, k } => {
            let bits = const_immediate_bits(&consts[*k as usize]).expect("eligibility checked");
            let v = cg.b.ins().iconst(types::I64, bits as i64);
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Move { dst, src } => {
            // The interpreter's `Move` retains the source then overwrites the destination. A call-free
            // prototype's `src` is always an immediate (retain is a no-op); a `heap_aware` one retains
            // a moved heap value, and `store_reg` releases the overwritten destination.
            let v = cg.load_reg(*src);
            if cg.heap_aware {
                cg.retain_if_heap(v);
            }
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Drop { reg, relevant } => {
            // Take the value out (leaving `unit`) and drop it. Immediate in a call-free prototype (no
            // release). In a `heap_aware` prototype a heap value is released — through the
            // destructor-aware path when the drop is IR-relevant, else the plain release.
            let v = cg.load_reg(*reg);
            let unit =
                cg.b.ins()
                    .iconst(types::I64, Value::NANBOX.unit_bits as i64);
            cg.store_reg_raw(*reg, unit);
            if cg.heap_aware {
                cg.release_dropped_if_heap(v, *relevant);
            }
            next(cg);
        }
        Op::Binary { op, dst, a, b, .. } => emit_binary(cg, *op, *dst, *a, *b, pc, op_blocks),
        Op::Jump { target } => {
            cg.b.ins().jump(op_blocks[*target as usize], &[]);
        }
        Op::JumpIfTrue { reg, target } => {
            // Taken iff the value is exactly `true`; a non-bool is simply not taken (the interpreter's
            // `as_bool() == Some(true)`), so no guard/bail is needed.
            let v = cg.load_reg(*reg);
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
            let v = cg.load_reg(*reg);
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
            // false → jump target; true → fall through; anything else → bail so the interpreter
            // raises E0007 ("`if` condition must be a bool").
            let v = cg.load_reg(*reg);
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
            let here = cg.pc_const(pc);
            cg.b.ins().return_(&[here]);
        }
        Op::LoadGlobal { dst, global, .. } => emit_load_global(cg, *dst, global.0, pc, op_blocks),
        Op::StoreGlobal { global, src } => emit_store_global(cg, global.0, *src, pc, op_blocks),
        Op::TakeGlobal { dst, global, .. } => emit_take_global(cg, *dst, global.0, pc, op_blocks),
        Op::Call { .. } => emit_call(cg, pc, op_blocks),
        Op::Return { src } => emit_return(cg, *src),
        op if is_leaf_heap_op(op) => emit_leaf_op(cg, pc, op_blocks),
        // A bail point (`is_fast_op` was checked at the top; unreachable in practice).
        _ => {
            let here = cg.pc_const(pc);
            cg.b.ins().return_(&[here]);
        }
    }
}

/// A native `Call` (P-JIT J3): hand the whole call to the `jit_call` runtime helper, which reads the
/// `Op::Call` back from `proto`/`pc` and runs the shared closure-call setup on the interpreter's
/// frame/register stacks (pushing the callee frame). Whatever it returns — `CALLED` (frame pushed),
/// a resume pc (a synchronous first-class-builtin call completed), or `ABORTED` — becomes this
/// compiled function's outcome, so the interpreter runs the callee and resumes the caller in tier 0.
fn emit_call(cg: &mut Codegen, pc: usize, op_blocks: &[Block]) {
    let vm = cg.vm;
    let frames = cg.frames;
    let regs_vec = cg.regs_vec;
    let base = cg.base;
    let proto = cg.b.ins().iconst(types::I32, cg.proto as i64);
    let pcv = cg.b.ins().iconst(types::I32, pc as i64);

    // Try a direct native→native call: `prepare_call` returns the callee's compiled entry pointer if
    // the call is direct-able (compiled callee, plain arity, no upvalues, stack capacity), else 0.
    let prep = cg.prepare_call_ref;
    let pinst =
        cg.b.ins()
            .call(prep, &[vm, frames, regs_vec, base, proto, pcv]);
    let fnptr = cg.b.inst_results(pinst)[0];
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
    cg.b.ins().return_(&[outcome]);

    // Direct: call the callee's compiled entry on the shared stack. `prepare_call` already reserved
    // the callee window and pushed its frame; `regs`/`globals`/`frames`/`regs_vec` pass through, the
    // callee base comes from `callee_base`, and `entry_pc = 0` (a fresh frame). No reallocation can
    // happen (capacity was checked), so `cg.regs` stays valid across the indirect call.
    cg.b.switch_to_block(direct);
    let cbinst = cg.b.ins().call(cg.callee_base_ref, &[vm]);
    let callee_base = cg.b.inst_results(cbinst)[0];
    let regs = cg.regs;
    let globals = cg.globals;
    let entry0 = cg.b.ins().iconst(types::I64, 0);
    let iinst = cg.b.ins().call_indirect(
        cg.callee_sig,
        fnptr,
        &[vm, regs, callee_base, globals, frames, regs_vec, entry0],
    );
    let callee_outcome = cg.b.inst_results(iinst)[0];
    // `after_call` interprets the callee outcome: continue in place, or an outcome to propagate.
    let ainst =
        cg.b.ins()
            .call(cg.after_call_ref, &[vm, frames, callee_outcome]);
    let after = cg.b.inst_results(ainst)[0];
    let cont = cg.b.ins().iconst(types::I64, OUTCOME_CONTINUE);
    let is_cont = cg.b.ins().icmp(IntCC::Equal, after, cont);
    let continue_blk = cg.b.create_block();
    let return_blk = cg.b.create_block();
    cg.b.ins().brif(is_cont, continue_blk, &[], return_blk, &[]);
    cg.b.switch_to_block(continue_blk);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
    cg.b.switch_to_block(return_blk);
    cg.b.ins().return_(&[after]);
}

/// A native leaf heap/collection op (P-JIT J4): run it through the `run_leaf_op` helper, which does
/// the interpreter's exact logic (refcounts included) and returns `OUTCOME_CONTINUE` (done — continue
/// to `pc + 1`) or a resume pc (it bailed — a dispatch or an error the interpreter must handle).
fn emit_leaf_op(cg: &mut Codegen, pc: usize, op_blocks: &[Block]) {
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
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
    cg.b.switch_to_block(return_blk);
    cg.b.ins().return_(&[outcome]);
}

/// A native `Op::Return` (P-JIT J3): hand the return value to the `jit_return` helper, which runs the
/// shared return protocol (transfer to the caller's destination, pop this frame) on the interpreter's
/// stacks, and propagate its outcome (`RETURNED`, or `HALTED` for the bottom frame). Value-returning
/// so a native direct caller gets its callee's result back without a bail.
fn emit_return(cg: &mut Codegen, src: Reg) {
    let raw = cg.load_reg(src);
    let vm = cg.vm;
    let frames = cg.frames;
    let regs_vec = cg.regs_vec;
    let f = cg.return_ref;
    let inst = cg.b.ins().call(f, &[vm, frames, regs_vec, raw]);
    let outcome = cg.b.inst_results(inst)[0];
    cg.b.ins().return_(&[outcome]);
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
    let here = cg.pc_const(pc);
    cg.b.ins().return_(&[here]);
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
    let here = cg.pc_const(pc);
    cg.b.ins().return_(&[here]);

    // Not a heap old value: safe to mutate. Take the source out (moved into the global — no release,
    // its reference transfers) and write the slot; a first bind also records it in `global_order`.
    cg.b.switch_to_block(cont);
    let v = cg.load_reg(src);
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
    let here = cg.pc_const(pc);
    cg.b.ins().return_(&[here]);
    cg.b.switch_to_block(cont);
    let unit =
        cg.b.ins()
            .iconst(types::I64, Value::NANBOX.unit_bits as i64);
    cg.store_global(g, unit);
    cg.store_reg(dst, old);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// Emit a numeric `Binary`, dispatching on the operands' runtime types (the bytecode is untyped):
/// both small ints → the integer fast path (J1); both f64 floats → the float fast path (J2);
/// anything else (mixed int/float, f32, objects, …) → bail to the interpreter, which handles the
/// widening/coercion and any type error.
fn emit_binary(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    a: Reg,
    b: Reg,
    pc: usize,
    op_blocks: &[Block],
) {
    let va = cg.load_reg(a);
    let vb = cg.load_reg(b);

    let a_int = cg.is_small_int(va);
    let b_int = cg.is_small_int(vb);
    let both_int = cg.b.ins().band(a_int, b_int);

    let int_block = cg.b.create_block();
    let float_check = cg.b.create_block();
    cg.b.ins().brif(both_int, int_block, &[], float_check, &[]);

    // Integer fast path.
    cg.b.switch_to_block(int_block);
    emit_int_binary(cg, op, dst, va, vb, pc, op_blocks);

    // Float fast path (or bail): both operands must be f64 floats.
    cg.b.switch_to_block(float_check);
    let a_flt = cg.is_float(va);
    let b_flt = cg.is_float(vb);
    let both_flt = cg.b.ins().band(a_flt, b_flt);
    let float_block = cg.b.create_block();
    guard(cg, both_flt, float_block, pc);
    emit_float_binary(cg, op, dst, va, vb, pc, op_blocks);
}

/// The integer body of a `Binary`, entered with both operands proven small ints (J1). Computes in
/// i64 with the interpreter's wrapping/trapping semantics and stores the boxed result — bailing
/// before any write on a zero divisor, a signed overflow, or an out-of-immediate-range result.
fn emit_int_binary(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    va: ClValue,
    vb: ClValue,
    pc: usize,
    op_blocks: &[Block],
) {
    let x = cg.unbox_int(va);
    let y = cg.unbox_int(vb);

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
            // Select the exact `true`/`false` NaN-box bits from the i1 comparison result.
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
/// 48 bits must reproduce the value.
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

/// Emit a fast-path guard: `brif cond -> cont else bail(pc)`, fill the bail block (which hands
/// control back to the interpreter at `pc`), and leave the builder positioned in `cont` so the
/// caller keeps emitting the fast path. `cont` is a caller-created block; `cond` is the
/// keep-going condition (true = stay in native code). No sealing here — [`FunctionBuilder`] uses no
/// SSA variables in this codegen (all state is in memory), so `seal_all_blocks` at the end suffices.
fn guard(cg: &mut Codegen, cond: ClValue, cont: Block, pc: usize) {
    let bail = cg.b.create_block();
    cg.b.ins().brif(cond, cont, &[], bail, &[]);
    cg.b.switch_to_block(bail);
    let here = cg.pc_const(pc);
    cg.b.ins().return_(&[here]);
    cg.b.switch_to_block(cont);
}
