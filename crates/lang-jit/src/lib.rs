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

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, Block, InstBuilder, MemFlagsData, Value as ClValue, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _, default_libcall_names};

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
/// - The `u32` return is the **bytecode `pc` at which the interpreter should resume** (the deopt
///   contract above). `0` means "interpret the whole frame" (an ineligible prototype's bail stub).
///
/// `extern "C"` so Cranelift's platform calling convention matches the pointer this is transmuted
/// from.
pub type CompiledFn = unsafe extern "C" fn(vm: *mut c_void, regs: *mut Value, base: usize) -> u32;

/// The name the bail stub calls to prove the runtime-helper ABI links. The VM registers a pointer
/// for this symbol when it constructs the [`Jit`]; J4 registers the real `retain`/`release`/`call`
/// helpers alongside it under the same convention.
pub const OBSERVE_HELPER: &str = "lang_jit_observe";

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
        let module = JITModule::new(builder);
        let ctx = module.make_context();
        Ok(Jit {
            module,
            compiled: Vec::new(),
            native_count: 0,
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

    /// The tier-1 ABI signature: `(vm: ptr, regs: ptr, base: usize) -> u32`.
    fn abi_signature(&self) -> cranelift_codegen::ir::Signature {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(ptr_ty)); // vm
        sig.params.push(AbiParam::new(ptr_ty)); // regs
        sig.params.push(AbiParam::new(ptr_ty)); // base (usize == pointer width)
        sig.returns.push(AbiParam::new(types::I32)); // resume pc
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
        let ptr_ty = self.module.target_config().pointer_type();
        let mut helper_sig = self.module.make_signature();
        helper_sig.params.push(AbiParam::new(ptr_ty));
        let helper_id = self
            .module
            .declare_function(OBSERVE_HELPER, Linkage::Import, &helper_sig)
            .map_err(|e| e.to_string())?;

        self.module.clear_context(&mut self.ctx);
        self.ctx.func.signature = self.abi_signature();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fb_ctx);
            let block = b.create_block();
            b.append_block_params_for_function_params(block);
            b.switch_to_block(block);
            b.seal_block(block);
            let vm = b.block_params(block)[0];
            let helper_ref = self.module.declare_func_in_func(helper_id, b.func);
            b.ins().call(helper_ref, &[vm]);
            let zero = b.ins().iconst(types::I32, 0);
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
        self.ctx.func.signature = self.abi_signature();
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fb_ctx);

            // The entry block holds the function params; one block per bytecode pc follows.
            let entry = b.create_block();
            let op_blocks: Vec<Block> = (0..n).map(|_| b.create_block()).collect();

            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let regs = b.block_params(entry)[1];
            let base = b.block_params(entry)[2];
            // frame_ptr = regs + base * 8 (Value is 8 bytes). All register access is off this.
            let base_bytes = b.ins().imul_imm(base, 8);
            let frame_ptr = b.ins().iadd(regs, base_bytes);

            let mut cg = Codegen {
                b: &mut b,
                frame_ptr,
            };

            // Parameter guard: if any argument is a heap pointer, bail to pc 0 (interpret the frame).
            // Establishes the "every register is an immediate" invariant the body relies on.
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
                    let zero = cg.b.ins().iconst(types::I32, 0);
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
                    let here = cg.b.ins().iconst(types::I32, pc as i64);
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

/// Whether every op in `chunk` is in the J1 integer subset (so it can be native-compiled). `Return`
/// and `Halt` count as eligible (they are bail points, not compiled ops). A `LoadConst` is eligible
/// only if its constant is an immediate (int in the 48-bit range, bool, unit, or a float — never a
/// heap string/module or a big int that would box).
fn is_eligible(chunk: &lang_bytecode::Chunk) -> bool {
    chunk.code.iter().all(|op| match op {
        Op::LoadConst { k, .. } => const_immediate_bits(&chunk.consts[*k as usize]).is_some(),
        Op::Move { .. } | Op::Drop { .. } => true,
        Op::Binary { op, .. } => supported_binary(*op),
        Op::Jump { .. }
        | Op::JumpIfTrue { .. }
        | Op::JumpIfFalse { .. }
        | Op::CondBranch { .. }
        | Op::Return { .. }
        | Op::Halt => true,
        _ => false,
    })
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

/// Forward reachability of each bytecode pc from pc 0, following the control-flow edges of the J1 op
/// set. Used so the codegen fills unreachable blocks (dead code) with a trivial bail instead of code
/// that would reference the entry-only frame pointer from a non-dominated block.
fn reachable_pcs(chunk: &lang_bytecode::Chunk) -> Vec<bool> {
    let n = chunk.code.len();
    let mut seen = vec![false; n];
    let mut stack = vec![0usize];
    while let Some(pc) = stack.pop() {
        if pc >= n || seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &chunk.code[pc] {
            Op::Jump { target } => stack.push(*target as usize),
            Op::JumpIfTrue { target, .. } | Op::JumpIfFalse { target, .. } => {
                stack.push(*target as usize);
                stack.push(pc + 1);
            }
            Op::CondBranch { target, .. } => {
                stack.push(*target as usize);
                stack.push(pc + 1);
            }
            Op::Return { .. } | Op::Halt => {}
            _ => stack.push(pc + 1),
        }
    }
    seen
}

/// A thin wrapper over the Cranelift builder carrying the frame base pointer — the context every
/// op-emitter needs. Keeps [`emit_op`] free of builder plumbing. Register access uses *trusted*
/// memory flags (aligned — the `Vec<Value>` is 8-byte aligned and every slot is at an 8-byte offset
/// — and non-trapping, since the compiler proved every register in range), so Cranelift emits a bare
/// load/store.
struct Codegen<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    frame_ptr: ClValue,
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

    /// Store `v` into register `r`. Sound with no release only under the immediate invariant (the
    /// old occupant is always an immediate, so the interpreter's release-on-overwrite is a no-op).
    fn store_reg(&mut self, r: Reg, v: ClValue) {
        self.b
            .ins()
            .store(MemFlagsData::trusted(), v, self.frame_ptr, reg_offset(r));
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

    /// A native i32 constant (a resume pc).
    fn pc_const(&mut self, pc: usize) -> ClValue {
        self.b.ins().iconst(types::I32, pc as i64)
    }
}

/// Register `r`'s byte offset within the frame window (`r * sizeof(Value)`).
fn reg_offset(r: Reg) -> i32 {
    (r as i32) * 8
}

/// Emit the native code for one op into its (already switched-to) block. `op_blocks[pc]` maps a
/// bytecode pc to its Cranelift block, for jumps/branches; a bail returns the pc.
fn emit_op(cg: &mut Codegen, consts: &[Const], op: &Op, pc: usize, op_blocks: &[Block]) {
    let next = |cg: &mut Codegen| cg.b.ins().jump(op_blocks[pc + 1], &[]);
    match op {
        Op::LoadConst { dst, k } => {
            let bits = const_immediate_bits(&consts[*k as usize]).expect("eligibility checked");
            let v = cg.b.ins().iconst(types::I64, bits as i64);
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Move { dst, src } => {
            // Under the immediate invariant `src` is an immediate, so a bit copy (no retain/release)
            // reproduces the interpreter's refcounted move exactly.
            let v = cg.load_reg(*src);
            cg.store_reg(*dst, v);
            next(cg);
        }
        Op::Drop { reg, .. } => {
            // The dropped value is an immediate (invariant) → no `release`/`destruct`; just clear the
            // slot to `unit`, matching the interpreter's `mem::replace(reg, unit)`.
            let unit =
                cg.b.ins()
                    .iconst(types::I64, Value::NANBOX.unit_bits as i64);
            cg.store_reg(*reg, unit);
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
        // Return / Halt / anything else: hand back to the interpreter at this pc.
        _ => {
            let here = cg.pc_const(pc);
            cg.b.ins().return_(&[here]);
        }
    }
}

/// Emit an integer `Binary`: guard both operands are small ints (bail otherwise), compute in i64
/// with the interpreter's wrapping/trapping semantics, and store the boxed result — bailing before
/// any write on a divisor-zero, a signed-overflow, or an out-of-immediate-range result.
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
    let both = cg.b.ins().band(a_int, b_int);

    // Guard: both operands small ints, else bail (the interpreter handles objects/floats/errors).
    let compute = cg.b.create_block();
    guard(cg, both, compute, pc);
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
