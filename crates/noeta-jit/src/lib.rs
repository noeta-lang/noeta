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

mod analysis;
mod plan;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, MemFlagsData, Type, Value as ClValue, types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{
    DataDescription, FuncId, Linkage, Module as ClifModule, default_libcall_names,
};
use cranelift_object::{ObjectBuilder, ObjectModule};

use noeta_ast::BinaryOp;
use noeta_bytecode::{Const, Module, Op, Reg};
use noeta_value::Value;

use analysis::*;

/// Declare one `Linkage::Import` runtime-helper symbol from its (name, params, returns) row —
/// the table-driven form of `from_module`'s eleven per-helper signature blocks (audit-1
/// finding 11). The pointer type is target-dependent, so callers pass fully-resolved
/// [`Type`]s.
fn declare_import<M: ClifModule>(
    module: &mut M,
    name: &str,
    params: &[Type],
    returns: &[Type],
) -> Result<FuncId, String> {
    let mut sig = module.make_signature();
    sig.params.extend(params.iter().map(|&t| AbiParam::new(t)));
    sig.returns
        .extend(returns.iter().map(|&t| AbiParam::new(t)));
    module
        .declare_function(name, Linkage::Import, &sig)
        .map_err(|e| e.to_string())
}

// The cranelift-free ABI contract (frame layout, `CompiledFn`, the `noeta_jit_*` helper names, the
// `CallSiteCache` shape, the `OUTCOME_*` sentinels, the AOT dispatch symbol + name helpers) lives in
// `noeta-jit-abi` so an AOT binary can link the runtime support without pulling Cranelift. Re-exported
// here so every `noeta_jit::*` path — and this crate's own bare references — resolve unchanged.
pub use noeta_jit_abi::{
    AFTER_CALL_HELPER, AOT_DISPATCH_SYMBOL, CALL_HELPER, CallSiteCache, CompiledFn, FMOD_HELPER,
    FrameLayout, LEAF_OP_HELPER, NOTE_GLOBAL_BOUND_HELPER, OBSERVE_HELPER, OUTCOME_ABORTED,
    OUTCOME_CALLED, OUTCOME_CONTINUE, OUTCOME_HALTED, OUTCOME_RETURNED, PREPARE_CALL_HELPER,
    RELEASE_HELPER, RELEASE_VALUE_HELPER, RETAIN_HELPER, RETURN_HELPER, SITE_EMPTY, SITE_POISON,
    fast_symbol, proto_symbol, stub_symbol,
};

/// Per-phase compile accounting (P-JCT C0): where the engine's total compile time goes, plus the
/// volume compiled — enough to compute a bytes/s throughput comparable against Cranelift's
/// expected range. `define_ns` covers `Module::define_function` (lowering, regalloc, and the IR
/// verifier when enabled); `finalize_ns` covers `finalize_definitions` (relocations + W^X page
/// flips, currently paid **per body**); IR construction is the remainder of the engine's
/// `compile_ns_total`. `bodies` counts finalized functions (classic + fast + bail stubs — more
/// than protos compiled), `clif_insts` the clif instructions handed to `define_function`, and
/// `code_bytes` the finalized machine code emitted.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompileBreakdown {
    pub define_ns: u64,
    pub finalize_ns: u64,
    pub bodies: u64,
    pub clif_insts: u64,
    pub code_bytes: u64,
}

/// The method JIT: a Cranelift [`JITModule`] plus a per-prototype cache of finalized entry points.
///
/// The cache is indexed by prototype index (into [`noeta_bytecode::Module::protos`]) — the same key
/// the interpreter dispatches on. `compiled[p]` is `Some` once prototype `p` has been JIT-compiled;
/// the interpreter consults it at frame entry.
pub struct Jit<M: ClifModule = JITModule> {
    /// The Cranelift module backend. Generic over `cranelift_module::Module` (P-AOT L3.0): the
    /// runtime JIT uses `JITModule` (owns finalized machine-code pages — must outlive every
    /// [`CompiledFn`] handed out), and the ahead-of-time path uses `cranelift_object::ObjectModule`
    /// (accumulates the same bodies into a relocatable object file). Every emit routine drives this
    /// through the `Module` trait alone, so the *same* IR construction targets both — monomorphized,
    /// so the JIT path keeps zero dispatch overhead.
    module: M,
    /// Finalized entry points, keyed by prototype index. `None` = not (yet) compiled → tier 0.
    compiled: Vec<Option<CompiledFn>>,
    /// Finalized **region-scoped** (OSR-window) bodies, keyed by prototype index — see
    /// [`OsrBody`] and [`Jit::compile_osr`]. `None` = no region body (never asked for, or the
    /// prototype's hot region is the whole prototype, where the main body already is the window).
    osr_compiled: Vec<Option<OsrBody>>,
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
    /// The run's **cancellation flag**, when it has one ([`Jit::new`]'s `cancel`) — the same
    /// `Arc<AtomicBool>` the interpreter polls at its own safepoints. Its address is baked into
    /// every loop header this engine compiles (see [`emit_cancel_poll`]); `None` — every ordinary
    /// run — emits no poll at all, so an uncancellable program's generated code is byte-identical
    /// to the pre-poll JIT.
    ///
    /// **This clone is the lifetime guarantee, and it is why the flag is stored rather than only
    /// read.** Generated code holds the `AtomicBool`'s address as a bare immediate; that address
    /// must stay valid for as long as any instruction that loads it can execute. Owning a strong
    /// reference here ties the flag to the very object that owns the code pages: the field is
    /// declared *after* `module`, so on drop the pages go first and this reference is released
    /// second — the flag strictly outlives every instruction that reads it, whatever the VM-side
    /// owners (a worker's parent, `RunOptions::cancel`) do with their own clones. In particular
    /// `Vm::observe_cancel` drops the VM's clone the instant a request is honored, and native code
    /// still running out of an older frame keeps polling a live flag.
    cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// [`Jit::cancel_flag`]'s address, or `0` for "no poll" — the immediate the codegen bakes.
    /// Derived once at construction so no compile has to reach through the `Option`.
    cancel_addr: u64,
    /// How many prototypes were compiled to *real* native code (vs a bail stub) — the coverage stat.
    native_count: usize,
    /// P-AOT L3.1 **dev oracle knob** (`NOETA_JIT_AOT=1`): make the *runtime* JIT emit its bodies in
    /// AOT mode (inline caches off, null call sites) instead of the production IC-on form. Semantics
    /// are identical — the IC-off path is the always-correct helper slow path — so the jit-differential
    /// run under this knob proves the ahead-of-time codegen is byte-identical in behaviour across the
    /// whole corpus, before any object-linking work exists. Default `false` (production JIT unchanged).
    aot_bodies: bool,
    /// Total / worst-case wall time spent inside [`Jit::compile`] (P-PAR S0c): every compile runs
    /// synchronously on the mutator thread today, so these are the pauses the program felt. Cache
    /// hits don't count — only actual codegen work.
    compile_ns_total: u64,
    compile_ns_max: u64,
    /// Where `compile_ns_total` actually goes (P-JCT C0) — see [`CompileBreakdown`].
    breakdown: CompileBreakdown,
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
    fmod_id: FuncId,
    ctx: cranelift_codegen::Context,
    fb_ctx: FunctionBuilderContext,
}

impl<M: ClifModule> std::fmt::Debug for Jit<M> {
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

impl<M: ClifModule> Jit<M> {
    /// Build the host target ISA under the JIT's codegen settings (P-AOT L3.0). Shared by every
    /// backend, so the AOT object path compiles under the *same* flags as the runtime JIT.
    ///
    /// `NOETA_JIT_OPT` is a **dev measurement knob** (P-PAR S4): it lets the compile-time /
    /// code-quality trade be A/B'd without a rebuild (`none` compiles far faster, `speed` runs
    /// faster). Semantics are identical at every level, so the jit-differential is unaffected; the
    /// shipped default stays `speed`. `NOETA_JIT_VERIFY=1|0` overrides the verifier (default: on
    /// under `debug_assertions`, off in release — P-JCT C1).
    fn make_isa(is_pic: bool) -> Result<cranelift_codegen::isa::OwnedTargetIsa, String> {
        let mut flags = settings::builder();
        flags
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        // The runtime JIT emits absolute-addressed code into its own W^X pages (`is_pic=false`); the
        // AOT object path wants position-independent, relocatable code (`is_pic=true`) so the linker
        // can place it. The JIT keeps `false` — its codegen is byte-identical to pre-L3.0.
        flags
            .set("is_pic", if is_pic { "true" } else { "false" })
            .map_err(|e| e.to_string())?;
        // P-JSSA: the SSA promotion leans on Cranelift's mid-end (block-param coalescing, GVN,
        // dead-load removal of unused entry inits). The default `opt_level=none` was fine for the
        // memory-form codegen; with block params it is not.
        let opt_level = std::env::var("NOETA_JIT_OPT").unwrap_or_else(|_| "speed".to_string());
        flags
            .set("opt_level", &opt_level)
            .map_err(|e| e.to_string())?;
        // P-JCT C1: Cranelift's IR verifier defaults **on** and re-checks the function at every
        // pass boundary — a pure debug tool (it never changes codegen) that dominated compile
        // time. Debug builds (the test suites, the jit-differential oracle in CI) keep it as a
        // safety net; release builds turn it off.
        let verify = match std::env::var("NOETA_JIT_VERIFY") {
            Ok(v) => v != "0",
            Err(_) => cfg!(debug_assertions),
        };
        flags
            .set("enable_verifier", if verify { "true" } else { "false" })
            .map_err(|e| e.to_string())?;
        // **Every compiled body must start at an even address** ([`noeta_jit_abi::MIN_BODY_ALIGNMENT`]).
        // The S4.1 direct-call protocol tags the fast convention in *bit 0* of the entry pointer —
        // `jit_prepare_call` returns `ff | FAST_ENTRY_TAG` and the caller strips it with `& !1` — so
        // an odd entry makes the tag indistinguishable from the address's own low bit and the caller
        // jumps **one byte before** the real body.
        //
        // Nothing gave us that for free: on x86-64 Cranelift's `function_alignment().minimum` and
        // `symbol_alignment()` are both **1**, so `cranelift-object` packs bodies back to back and a
        // body whose predecessor ends on an odd boundary lands odd. That is exactly how a linked
        // `--native` artifact crashed (`modules/derived_package_path`): a leaf prototype's fast body
        // sat at an odd address, `ff | 1` was a no-op, and the caller called `ff - 1`, whose `ret`
        // handed that address straight back as the callee's "outcome" — which `jit_after_call` then
        // wrote into the callee frame as a bytecode pc. The runtime JIT never hit it only because its
        // per-body `finalize_definitions` hands out freshly aligned code memory; the property was
        // luck, not contract, on both paths.
        //
        // 16 is the ordinary function alignment for this target (Cranelift's own `preferred` is 32);
        // it costs a few padding bytes per body and makes the tag's precondition structural.
        flags
            .set(
                "log2_min_function_alignment",
                &noeta_jit_abi::MIN_BODY_ALIGNMENT
                    .trailing_zeros()
                    .to_string(),
            )
            .map_err(|e| e.to_string())?;
        let isa_builder = cranelift_native::builder().map_err(|m| m.to_string())?;
        isa_builder
            .finish(settings::Flags::new(flags))
            .map_err(|e| e.to_string())
    }

    /// Finish building a `Jit<M>` around an already-constructed `module`: declare the runtime-helper
    /// imports and the codegen context (P-AOT L3.0). Everything here is `Module`-trait-only, so both
    /// the runtime JIT (`JITModule`, via [`Jit::new`]) and the AOT object backend (`ObjectModule`,
    /// via [`Jit::new_object`]) share it. The helper `FuncId`s it returns are `Linkage::Import`
    /// symbols: the JIT resolves them to the registered Rust `extern "C"` pointers, the object path
    /// leaves them as relocations for the final link against the runtime crate.
    fn from_module(
        mut module: M,
        layout: FrameLayout,
        frame_template: *const u8,
    ) -> Result<Jit<M>, String> {
        if !layout.frame_size.is_multiple_of(8) {
            return Err("Frame size must be word-aligned for the native frame push".to_string());
        }
        let ptr_ty = module.target_config().pointer_type();
        // The 11 runtime-helper imports, each one row of (name, params, returns) through
        // `declare_import` (audit-1 finding 11 — was ~100 lines of per-helper signature
        // boilerplate). The parameter conventions:
        // - `observe(vm: ptr)`; `note_global_bound(vm: ptr, g: i32)`.
        // - Heap/call helpers (J3): `retain(v: i64)`, `release(v: i64)`,
        //   `release_value(vm: ptr, v: i64)`,
        //   `call(vm, frames, regs_vec: ptr, base: usize, proto: i32, pc: i32) -> i64`.
        // - `return(vm, frames, regs_vec: ptr, raw: i64, release_mask: i64) -> i64` — the mask
        //   is the S4.0 fast teardown: which window slots may hold heap values at this site.
        // - Direct-call helpers: `prepare_call` takes `call`'s params plus a site cache slot
        //   (S4.2, or null) and returns two words — (fnptr-or-0, callee window base), the VM's
        //   `#[repr(C)] PreparedCall` (rax:rdx under SysV, exactly what a two-i64-return
        //   Cranelift import reads back); `after_call(vm, frames, outcome: i64) -> i64`.
        // - `run_leaf_op(vm, regs_vec: ptr, base: usize, proto: i32, pc: i32) -> i64`.
        // - `fmod(a: f64, b: f64) -> f64` — float `%` (S2).
        let (p, i32t, i64t, f64t) = (ptr_ty, types::I32, types::I64, types::F64);
        let call_params = [p, p, p, p, i32t, i32t];
        let mut prepare_params = call_params.to_vec();
        prepare_params.push(p); // site cache slot (S4.2), or null
        let observe_id = declare_import(&mut module, OBSERVE_HELPER, &[p], &[])?;
        let note_bound_id = declare_import(&mut module, NOTE_GLOBAL_BOUND_HELPER, &[p, i32t], &[])?;
        let retain_id = declare_import(&mut module, RETAIN_HELPER, &[i64t], &[])?;
        let release_id = declare_import(&mut module, RELEASE_HELPER, &[i64t], &[])?;
        let release_value_id = declare_import(&mut module, RELEASE_VALUE_HELPER, &[p, i64t], &[])?;
        let call_id = declare_import(&mut module, CALL_HELPER, &call_params, &[i64t])?;
        let return_id =
            declare_import(&mut module, RETURN_HELPER, &[p, p, p, i64t, i64t], &[i64t])?;
        let prepare_call_id = declare_import(
            &mut module,
            PREPARE_CALL_HELPER,
            &prepare_params,
            &[i64t, i64t],
        )?;
        let after_call_id = declare_import(&mut module, AFTER_CALL_HELPER, &[p, p, i64t], &[i64t])?;
        let leaf_op_id =
            declare_import(&mut module, LEAF_OP_HELPER, &[p, p, p, i32t, i32t], &[i64t])?;
        let fmod_id = declare_import(&mut module, FMOD_HELPER, &[f64t, f64t], &[f64t])?;
        let ctx = module.make_context();
        Ok(Jit {
            module,
            compiled: Vec::new(),
            osr_compiled: Vec::new(),
            fast_compiled: Vec::new(),
            layout,
            frame_template,
            site_slots: Vec::new(),
            // Armed by `Jit::new` when the run carries one; the AOT object path never does.
            cancel_flag: None,
            cancel_addr: 0,
            native_count: 0,
            compile_ns_total: 0,
            compile_ns_max: 0,
            breakdown: CompileBreakdown::default(),
            // Dev oracle knob (L3.1): make the runtime JIT emit AOT-form (IC-off) bodies so the
            // jit-differential can prove the ahead-of-time codegen byte-identical across the corpus.
            aot_bodies: std::env::var_os("NOETA_JIT_AOT").is_some(),
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
            fmod_id,
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

    /// Emit **AOT-form** bodies from here on (P-AOT L3.1): inline caches off, null call sites, no
    /// cancellation poll — the exact codegen [`Jit::new_object`] produces for a `noeta build
    /// --native` object, but finalized to executable pages so an ordinary in-process run exercises
    /// it. Set by the VM from [`RunOptions::aot_bodies`], which is how the JIT differential gets its
    /// AOT arm: same corpus, same comparison, second codegen shape.
    ///
    /// This is the *option* form of the `NOETA_JIT_AOT` environment knob [`Jit::new`] still honours.
    /// The knob arms a whole process (and cannot be set from inside a `#[test]` without an `unsafe`
    /// mutation of the process environment); this arms one run, which is what lets the arm be a
    /// per-commit `cargo test` gate rather than a shell-only one.
    ///
    /// Must be set **before** the first `compile` — bodies already emitted keep the form they were
    /// emitted in. `Vm::init_jit` sets it at construction, before the `force_jit` sweep.
    ///
    /// [`RunOptions::aot_bodies`]: ../noeta_vm/struct.RunOptions.html#structfield.aot_bodies
    pub fn set_aot_bodies(&mut self, on: bool) {
        self.aot_bodies = on;
    }

    /// Whether this engine emits AOT-form bodies — the env knob or [`Jit::set_aot_bodies`].
    pub fn aot_bodies(&self) -> bool {
        self.aot_bodies
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

    /// Total wall time spent compiling across the run, in nanoseconds (P-PAR S0c).
    pub fn compile_ns_total(&self) -> u64 {
        self.compile_ns_total
    }

    /// The single longest compile — the worst pause the mutator felt, in nanoseconds (P-PAR S0c).
    pub fn compile_ns_max(&self) -> u64 {
        self.compile_ns_max
    }

    /// Per-phase breakdown of `compile_ns_total` plus compiled volume (P-JCT C0).
    pub fn compile_breakdown(&self) -> CompileBreakdown {
        self.breakdown
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

    /// Declare + define the current `self.ctx` under `name` into the module, returning its
    /// [`FuncId`] — **without** finalizing it to an executable pointer. This is the
    /// backend-agnostic tail of body emission (P-AOT L3.0): it uses only `cranelift_module::Module`
    /// operations (`declare_function`/`define_function`/`clear_context`), so the *same* IR
    /// construction feeds both the runtime JIT (which then [`finalize_ptr`](Self::finalize_ptr)s to
    /// a code pointer) and an ahead-of-time [`cranelift_object::ObjectModule`] (which accumulates
    /// the defined body into an object file). The clif/define/code-byte accounting lives here; the
    /// finalize accounting lives in `finalize_ptr`, so a JIT compile (define + finalize) reproduces
    /// the exact per-phase breakdown the single `finalize` method used to record.
    fn define_body(&mut self, name: &str) -> Result<FuncId, String> {
        // Debug tool: `NOETA_JIT_DISASM=1` dumps each compiled prototype's final machine code
        // (vcode form: post-regalloc, real machine instructions) to stderr — the native analogue
        // of `noeta dump`, for inspecting what the JIT actually emits.
        let want_disasm = std::env::var_os("NOETA_JIT_DISASM").is_some();
        self.ctx.set_disasm(want_disasm);
        // Debug tool: `NOETA_JIT_CLIF=1` dumps each body's clif IR as handed to Cranelift —
        // pre-optimization, so it shows exactly what *our* emitter produced (the input-volume
        // side of the compile-throughput ledger; `NOETA_JIT_DISASM` shows the output side).
        if std::env::var_os("NOETA_JIT_CLIF").is_some() {
            eprintln!("=== {name} (clif) ===\n{}", self.ctx.func.display());
        }
        self.breakdown.clif_insts += self.ctx.func.dfg.num_insts() as u64;
        let define_start = std::time::Instant::now();
        let func_id = self
            .module
            .declare_function(name, Linkage::Export, &self.ctx.func.signature)
            .map_err(|e| e.to_string())?;
        self.module
            .define_function(func_id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
        self.breakdown.define_ns += define_start.elapsed().as_nanos() as u64;
        if let Some(code) = self.ctx.compiled_code() {
            self.breakdown.code_bytes += code.code_buffer().len() as u64;
            if want_disasm && let Some(vcode) = &code.vcode {
                eprintln!("=== {name} ===\n{vcode}");
            }
        }
        self.module.clear_context(&mut self.ctx);
        Ok(func_id)
    }

    /// Emit the bail stub for an ineligible prototype: call the `noeta_jit_observe` helper (proving
    /// the helper ABI links and the VM pointer round-trips) and return `0` — "interpret the whole
    /// frame".
    fn emit_bail_stub(&mut self, proto: usize) -> Result<FuncId, String> {
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
        self.define_body(&stub_symbol(proto))
    }

    /// Emit the native integer body for a J1-eligible prototype (see the module docs). One Cranelift
    /// block per bytecode `pc`; register state lives in memory (the `regs` array), so blocks carry no
    /// SSA params — only the frame base pointer, computed once in the entry block, crosses into them.
    fn emit_int_body(
        &mut self,
        module: &Module,
        proto: usize,
        fast: bool,
        aot: bool,
        region: Option<(usize, usize)>,
    ) -> Result<FuncId, String> {
        let chunk = &module.protos[proto];
        let n = chunk.code.len();
        // A fast body is entered only fresh at pc 0 (no seam resume, no OSR — the interpreter
        // re-enters a deopted fast frame through the normal body), and is never region-scoped.
        let reachable = if fast {
            reachable_pcs_from(chunk, vec![0], &module.names, None)
        } else {
            reachable_pcs_from(
                chunk,
                entry_pcs(chunk)
                    .into_iter()
                    .filter(|&pc| in_region(region, pc))
                    .collect(),
                &module.names,
                region,
            )
        };

        self.module.clear_context(&mut self.ctx);
        // S4.2 inline caches: one slot per call site in this body, allocated up front so their
        // (stable, boxed) addresses can be baked into the code below. An AOT body emits no
        // inline-cache path (those absolute addresses don't survive into a relocatable object —
        // L3.1), so it allocates no slots and leaves `site_addrs` zeroed.
        let mut site_addrs: Vec<u64> = vec![0; n];
        if !aot {
            for (pc, op) in chunk.code.iter().enumerate() {
                if matches!(op, Op::Call { .. } | Op::CallGlobal { .. }) {
                    let slot: Box<CallSiteCache> = Box::new([SITE_EMPTY, 0, 0, 0]);
                    site_addrs[pc] = slot.as_ref() as *const CallSiteCache as u64;
                    self.site_slots.push(slot);
                }
            }
        }
        let frame_template_addr = self.frame_template as u64;
        // The cancellation poll's two codegen inputs (isolate-cancel / JIT half). An AOT body
        // never polls: the flag is a per-process heap address that cannot be baked into a
        // relocatable object — the same reason the inline-cache slots are null there (L3.1) — and
        // an ahead-of-time binary has no cancellable run to poll for anyway.
        let cancel_addr = if aot { 0 } else { self.cancel_addr };
        let loop_header = if cancel_addr == 0 {
            Vec::new()
        } else {
            let mut h = vec![false; n];
            for (pc, op) in chunk.code.iter().enumerate() {
                if let Some(t) = backward_target(op, pc) {
                    h[t] = true;
                }
            }
            h
        };
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
            let pool = ConstPool::seed(&mut b);
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
            let fmod_ref = self.module.declare_func_in_func(self.fmod_id, b.func);
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
                    matches!(op, Op::Call { .. } | Op::CallGlobal { .. })
                        || writes_heap_reg(op, &chunk.consts)
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
                pool,
                bail_blocks: vec![None; n],
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
                cancel_addr,
                loop_header,
                aot,
                note_bound_ref,
                retain_ref,
                release_ref,
                release_value_ref,
                call_ref,
                return_ref,
                prepare_call_ref,
                after_call_ref,
                leaf_op_ref,
                fmod_ref,
                callee_sig,
                fast_sigs,
            };

            if let Some(entry_pc) = entry_pc {
                // Entry-pc dispatch (J3 resume-native): jump to the block for `entry_pc`. `0` is a
                // fresh frame (run the parameter guard first); a post-call resume pc jumps straight
                // to its block; any other value has no native entry, so bail (the interpreter runs
                // that frame). The valid resume pcs are exactly `call_pc + 1` for each `Call` (the
                // interpreter re-enters a frame only at pc 0 or just after a call returns).
                // A region-scoped body serves only the entries inside its window; anything else
                // falls to `bad_entry` and the interpreter (which reaches the whole-prototype body
                // through the VM's own routing).
                let resume_targets: Vec<usize> = entry_pcs(chunk)
                    .into_iter()
                    .filter(|&p| p != 0 && in_region(region, p))
                    .collect();
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
                if !in_region(region, 0) {
                    // A region-scoped body whose window starts past pc 0 has no fresh-frame entry
                    // at all: hand the frame straight back (resume pc 0 = "interpret this frame").
                    cg.b.ins().return_(&[cg.pool.zero]);
                } else {
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
                            cg.b.ins().return_(&[cg.pool.zero]);
                        }
                        None => {
                            cg.b.ins().jump(init0, &[]);
                        }
                    }
                    cg.b.switch_to_block(init0);
                    cg.load_ssa_vars();
                    cg.init_raw_vars(0);
                    cg.b.ins().jump(op_blocks[0], &[]);
                }
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
                        let unit = cg.pool.unit;
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
                        cg.b.ins().return_(&[cg.pool.zero, cg.pool.zero]);
                    }
                    None => {
                        cg.b.ins().jump(init0, &[]);
                    }
                }
                cg.b.switch_to_block(init0);
                let unit = cg.pool.unit;
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
                    cg.ret_bail_isolated(pc);
                    continue;
                }
                // The region's edge (P-OSRW): a reachable pc outside a region-scoped body's window
                // is a bail point exactly as an uncompilable op is — sync the window and hand the
                // pc back. This is what keeps a cold tail out of the loop's register allocation.
                if !in_region(region, pc) {
                    cg.cur_pc = pc;
                    cg.sync_frame(pc);
                    let here = cg.pc_const(pc);
                    cg.ret_bail(here);
                    continue;
                }
                // The cancellation safepoint (isolate-cancel, JIT half): a loop header is entered
                // once per iteration, so a poll here is the native analogue of the interpreter's
                // taken-back-edge poll. Emitted only when the run carries a flag; otherwise
                // `cancel_addr == 0` and not a byte of this reaches the body.
                if cg.cancel_addr != 0 && cg.loop_header[pc] {
                    emit_cancel_poll(&mut cg, pc);
                }
                emit_op(&mut cg, &chunk.consts, &module.names, op, pc, &op_blocks);
            }

            b.seal_all_blocks();
            b.finalize();
        }
        let name = match (fast, region) {
            (true, _) => fast_symbol(proto),
            (false, None) => proto_symbol(proto),
            // A region-scoped body is a second definition of the same prototype, so it needs its
            // own symbol. Runtime-only (an AOT compile never region-scopes), so this name is not
            // part of the linked-artifact contract `noeta_jit_abi`'s symbol helpers carry.
            (false, Some((lo, hi))) => format!("{}_osr{lo}_{hi}", proto_symbol(proto)),
        };
        self.define_body(&name)
    }
}

/// What an ahead-of-time compile produced for one prototype (P-AOT L3.1b) — its slot in an
/// [`AotManifest`], indexed by the prototype's dispatch key. `native == false` means the prototype
/// was left for the interpreter (ineligible), so no symbol was emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AotProtoEntry {
    /// Whether a real native body was emitted (vs. left interpreted).
    pub native: bool,
    /// The main body's export symbol, if `native` — see [`proto_symbol`].
    pub symbol: Option<String>,
    /// The fast-convention body's export symbol, if one was emitted — see [`fast_symbol`].
    pub fast_symbol: Option<String>,
}

/// The result of an ahead-of-time whole-module compile (P-AOT L3.1b): one [`AotProtoEntry`] per
/// prototype, in prototype-index order. The runtime binds each `native` entry's symbol back into its
/// per-proto dispatch tables at startup (L3.2); the shape is fully derivable from the `Module`
/// (eligibility + symbol naming), so it need not be serialized separately — it travels *as* the
/// module plus this naming contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AotManifest {
    /// One entry per prototype, in index order.
    pub protos: Vec<AotProtoEntry>,
}

impl AotManifest {
    /// How many prototypes were compiled to real native bodies (the AOT coverage stat).
    pub fn native_count(&self) -> usize {
        self.protos.iter().filter(|e| e.native).count()
    }
}

/// A finalized **region-scoped OSR body** (P-OSRW): a second native body for one prototype whose
/// compiled region is the closed pc window `[lo, hi]` of the hot loop a back-edge promotion entered
/// at, everything outside it a bail.
///
/// Why it exists: Cranelift allocates registers over a *whole function*, so a prototype's cold
/// prologue and tail compete with its hot loop for machine registers — measured, nativizing one op
/// in the tail of a top-level script made the loop spill a value that had lived in `%r14`, costing
/// ~5% on the loop. Compiling the window on its own removes the competition.
///
/// The window is also the routing key: the VM sends a native re-entry whose pc lies in `[lo, hi]`
/// here and every other entry (a fresh frame, a post-call resume in the prologue or tail) to the
/// whole-prototype body, so nothing loses tier-1 coverage. `lo`/`hi` are `u32` to match the
/// bytecode's own pc width.
#[derive(Debug, Clone, Copy)]
pub struct OsrBody {
    /// The finalized entry point — the ordinary [`CompiledFn`] ABI, entered with `entry_pc` set to
    /// the loop header (or any other native entry inside the window).
    pub entry: CompiledFn,
    /// First pc of the window (inclusive).
    pub lo: u32,
    /// Last pc of the window (inclusive).
    pub hi: u32,
}

impl OsrBody {
    /// Whether a native re-entry at `pc` belongs to this body's window.
    #[inline]
    pub fn covers(&self, pc: usize) -> bool {
        pc >= self.lo as usize && pc <= self.hi as usize
    }
}

/// The runtime tier-1 compile error: **compilation declined — interpret instead** (audit-1
/// finding 14). Deliberately zero-size: every runtime call site discards the reason and falls
/// back to tier-0 (always sound under the bail-before-mutate contract), so a formatted
/// `String` was cost without a consumer. The AOT/object surface (`new_object`,
/// `compile_object`, `compile_module`, `finish`) keeps `Result<_, String>` — `noeta build
/// --native` shows those messages to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDecline;

impl std::fmt::Display for JitDecline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tier-1 compilation declined; interpreting")
    }
}

impl std::error::Error for JitDecline {}

impl Jit<JITModule> {
    /// Build a runtime JIT engine, registering the runtime-helper symbols the generated code may
    /// call. Each `(name, ptr)` is a `*const u8` cast of an `extern "C"` Rust function the VM owns;
    /// Cranelift resolves calls to `name` against `ptr`. The VM passes at least [`OBSERVE_HELPER`].
    ///
    /// Returns [`JitDecline`] if the host ISA is unavailable or Cranelift rejects the flags —
    /// the VM treats that as "JIT unavailable, stay tier 0".
    ///
    /// `cancel` is the run's cancellation flag, or `None`. It is a **codegen input**, not a
    /// runtime switch: an engine built with one emits a cancellation poll at every loop header it
    /// compiles, an engine built without one emits none. That is deliberate — the flag is armed
    /// before the run starts (`RunOptions::cancel`, or a worker's inherited flag), so an engine
    /// never has to change its mind mid-run, and an ordinary program's native code is not merely
    /// *fast* but literally the same bytes as before the poll existed. See [`Jit::cancel_flag`]
    /// for the lifetime argument.
    pub fn new(
        helpers: &[(&str, *const u8)],
        layout: FrameLayout,
        frame_template: *const u8,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Jit<JITModule>, JitDecline> {
        let isa = Self::make_isa(false).map_err(|_| JitDecline)?;
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        for (name, ptr) in helpers {
            builder.symbol(*name, *ptr);
        }
        let module = JITModule::new(builder);
        let mut jit = Self::from_module(module, layout, frame_template).map_err(|_| JitDecline)?;
        jit.cancel_addr = cancel
            .as_ref()
            .map(|f| std::sync::Arc::as_ptr(f) as u64)
            .unwrap_or(0);
        jit.cancel_flag = cancel;
        Ok(jit)
    }

    /// Compile prototype `proto` of `module` and cache its entry point, returning it. A J1-eligible
    /// prototype gets a native integer body; anything else gets a bail stub (→ interpreted).
    /// Idempotent: a second call for an already-compiled prototype returns the cached entry point.
    ///
    /// This is the runtime-JIT driver: each body is defined ([`emit_int_body`](Self::emit_int_body)
    /// / [`emit_bail_stub`](Self::emit_bail_stub) → [`define_body`](Self::define_body)) and then
    /// immediately finalized to a code pointer via [`finalize_ptr`](Self::finalize_ptr) — the same
    /// per-body finalize order the pre-L3.0 `compile` used, so behaviour and the compile breakdown
    /// are unchanged. The AOT path (L3.1) reuses the *same* `emit_*` routines but defers finalize,
    /// emitting an object file instead.
    pub fn compile(&mut self, module: &Module, proto: usize) -> Result<CompiledFn, JitDecline> {
        self.compile_inner(module, proto).map_err(|_| JitDecline)
    }

    /// [`compile`](Self::compile)'s body, over the internals' `String` errors (shared with the
    /// AOT/object path, where the message reaches the user).
    fn compile_inner(&mut self, module: &Module, proto: usize) -> Result<CompiledFn, String> {
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
        // The dev oracle knob makes the runtime JIT emit AOT-form (IC-off) bodies — identical
        // semantics, so the differential run under `NOETA_JIT_AOT=1` validates the AOT codegen.
        let aot = self.aot_bodies;
        let f = if is_eligible(chunk, &module.names) {
            let main_id = self.emit_int_body(module, proto, false, aot, None)?;
            let f = self.finalize_ptr(main_id)?;
            self.native_count += 1;
            // S4.1: also compile the fast-convention body where the prototype supports the
            // frameless-window contract; direct calls to it then skip the window fill, the
            // argument copy, and the helper-side return protocol.
            if fast_ok(chunk) {
                let fast_id = self.emit_int_body(module, proto, true, aot, None)?;
                let ff = self.finalize_ptr(fast_id)?;
                self.fast_compiled[proto] = Some(ff as usize);
            }
            f
        } else {
            let stub_id = self.emit_bail_stub(proto)?;
            self.finalize_ptr(stub_id)?
        };
        self.compiled[proto] = Some(f);
        let ns = compile_start.elapsed().as_nanos() as u64;
        self.compile_ns_total += ns;
        self.compile_ns_max = self.compile_ns_max.max(ns);
        Ok(f)
    }

    /// Compile the **region-scoped OSR body** for a back-edge-born promotion of `proto` that got
    /// hot at loop header `header` (P-OSRW). The body's native region is the loop's own pc window
    /// ([`osr_region`]); every pc outside it is a bail, so Cranelift allocates registers for the
    /// loop instead of for the union of the loop and the prototype's cold prologue/tail.
    ///
    /// `None` — "no region body, use the whole-prototype one" — when:
    ///
    /// - `header` is not a loop header (nothing to scope to), or
    /// - the window is the whole prototype, where a second body would be the same code, or
    /// - the window holds no natively-compilable op (a bail-only body buys nothing).
    ///
    /// Idempotent per prototype: the first window wins, and a later back-edge reuses it (the VM
    /// takes one OSR per prototype anyway).
    pub fn compile_osr(&mut self, module: &Module, proto: usize, header: usize) -> Option<OsrBody> {
        if proto >= self.osr_compiled.len() {
            self.osr_compiled
                .resize(module.protos.len().max(proto + 1), None);
        }
        if let Some(b) = self.osr_compiled[proto] {
            return Some(b);
        }
        let chunk = &module.protos[proto];
        let (lo, hi) = osr_region(chunk, header)?;
        if lo == 0 && hi + 1 >= chunk.code.len() {
            return None; // the window IS the prototype — the main body already is the region body
        }
        if !chunk.code[lo..=hi]
            .iter()
            .any(|op| is_fast_op(op, &module.names))
        {
            return None;
        }
        let compile_start = std::time::Instant::now();
        let aot = self.aot_bodies;
        let id = self
            .emit_int_body(module, proto, false, aot, Some((lo, hi)))
            .ok()?;
        let entry = self.finalize_ptr(id).ok()?;
        self.compile_ns_total += compile_start.elapsed().as_nanos() as u64;
        let body = OsrBody {
            entry,
            lo: lo as u32,
            hi: hi as u32,
        };
        self.osr_compiled[proto] = Some(body);
        Some(body)
    }

    /// Finalize a defined `func_id` to its executable entry point — the JIT-only tail (relocations
    /// and the W^X page flip via `finalize_definitions`, then `get_finalized_function`). Split out of
    /// body emission (P-AOT L3.0) so the shared codegen ([`define_body`](Self::define_body)) carries
    /// no runtime-JIT dependency; the AOT path never calls this (it emits an object file instead).
    fn finalize_ptr(&mut self, func_id: FuncId) -> Result<CompiledFn, String> {
        let finalize_start = std::time::Instant::now();
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        self.breakdown.finalize_ns += finalize_start.elapsed().as_nanos() as u64;
        self.breakdown.bodies += 1;
        let code = self.module.get_finalized_function(func_id);
        // SAFETY: `code` is a finalized function whose Cranelift signature is built (in
        // `from_module`) to exactly the 7-parameter `noeta_jit_abi::CompiledFn` ABI —
        // `unsafe extern "C" fn(vm, regs, base, globals, frames, regs_vec, …) -> i64` — that this
        // transmutes to, and it stays valid for as long as `self.module` (which owns the code
        // page) lives.
        Ok(unsafe { std::mem::transmute::<*const u8, CompiledFn>(code) })
    }
}

impl Jit<ObjectModule> {
    /// Build an ahead-of-time object-file compiler (P-AOT L3.0): the *same* codegen as the runtime
    /// JIT, but bodies are accumulated into a relocatable object instead of finalized to executable
    /// pages. Runtime-helper calls become `Linkage::Import` relocations, resolved when the object is
    /// linked against the runtime crate (L3.2). `name` is the object's module name.
    ///
    /// AOT bodies are object-safe (L3.1a audit): the frame push bakes the template's *words* as
    /// position-independent immediates (not the template address), and the inline cache — the one
    /// per-process absolute address — is turned off in AOT mode, so calls route through the
    /// `prepare_call` helper (a `Linkage::Import` relocation). Running the linked bodies is L3.2.
    pub fn new_object(
        name: &str,
        layout: FrameLayout,
        frame_template: *const u8,
    ) -> Result<Jit<ObjectModule>, String> {
        let isa = Self::make_isa(true)?;
        let builder = ObjectBuilder::new(isa, name.to_string(), default_libcall_names())
            .map_err(|e| e.to_string())?;
        let module = ObjectModule::new(builder);
        Self::from_module(module, layout, frame_template)
    }

    /// Define prototype `proto`'s body into the object (native if J1-eligible, else a bail stub),
    /// returning its `FuncId`. Reuses the runtime JIT's `emit_*` routines verbatim — no finalize.
    pub fn compile_object(&mut self, module: &Module, proto: usize) -> Result<FuncId, String> {
        let chunk = &module.protos[proto];
        if is_eligible(chunk, &module.names) {
            self.emit_int_body(module, proto, false, true, None)
        } else {
            self.emit_bail_stub(proto)
        }
    }

    /// Eagerly compile **every** J1-eligible prototype of `module` into the object (P-AOT L3.1b),
    /// returning the [`AotManifest`] the runtime uses to bind the finished symbols back into its
    /// per-proto entry tables at startup. Unlike the runtime JIT (which compiles hot prototypes on
    /// demand), this is whole-module: every eligible prototype gets a native main body — and its
    /// fast-convention body where the S4.1 contract holds — as an exported symbol. Ineligible
    /// prototypes get **no** native entry (the runtime simply interprets them; no bail-stub
    /// round-trip), recorded as [`AotProtoEntry::native`] `= false`.
    ///
    /// Design note (hot-reload): the manifest is a proto-index → symbol map that populates the
    /// runtime's *mutable* entry tables — the same tables JIT compilation fills. AOT symbols are the
    /// *initial* population, not a frozen binding; a later hot-reload can re-point any proto's entry
    /// to a freshly (re)compiled body. Crucially, AOT calls route through the entry-table indirection
    /// (the `prepare_call` helper), **not** baked direct native→native call targets — so swapping a
    /// proto never requires patching its callers.
    pub fn compile_module(&mut self, module: &Module) -> Result<AotManifest, String> {
        let n = module.protos.len();
        let mut protos = Vec::with_capacity(n);
        // Collect the `FuncId`s as we emit, to relocate into the dispatch table below.
        let mut main_ids: Vec<Option<FuncId>> = vec![None; n];
        let mut fast_ids: Vec<Option<FuncId>> = vec![None; n];
        for (p, chunk) in module.protos.iter().enumerate() {
            let entry = if is_eligible(chunk, &module.names) {
                main_ids[p] = Some(self.emit_int_body(module, p, false, true, None)?);
                let fast = if fast_ok(chunk) {
                    fast_ids[p] = Some(self.emit_int_body(module, p, true, true, None)?);
                    Some(fast_symbol(p))
                } else {
                    None
                };
                AotProtoEntry {
                    native: true,
                    symbol: Some(proto_symbol(p)),
                    fast_symbol: fast,
                }
            } else {
                // Ineligible: no native entry — the runtime interprets this prototype directly.
                AotProtoEntry {
                    native: false,
                    symbol: None,
                    fast_symbol: None,
                }
            };
            protos.push(entry);
        }
        self.define_dispatch_table(&main_ids, &fast_ids)?;
        Ok(AotManifest { protos })
    }

    /// Emit the [`AOT_DISPATCH_SYMBOL`] data object (P-AOT L3.2, approach A): the proto-index →
    /// entry-pointer table the runtime binds into its dispatch tables at startup. Layout is
    /// pointer-width words: `[count][main_0, fast_0, main_1, fast_1, …]`, each function slot a
    /// **relocation** to that proto's exported body (`write_function_addr`), or null (a zeroed slot)
    /// for an interpreted proto or a proto with no fast body. The linker resolves the relocations
    /// when the object is linked into the final binary, so the runtime reads real code addresses out
    /// of this one exported static — no per-symbol `dlsym`, no dynamic-symbol table needed.
    fn define_dispatch_table(
        &mut self,
        main_ids: &[Option<FuncId>],
        fast_ids: &[Option<FuncId>],
    ) -> Result<(), String> {
        let n = main_ids.len();
        let w = self.module.target_config().pointer_bytes() as usize;
        // count word, then two pointer slots per proto.
        let mut bytes = vec![0u8; w * (1 + 2 * n)];
        bytes[0..8].copy_from_slice(&(n as u64).to_le_bytes());

        let mut data = DataDescription::new();
        // The table is read as `usize`/function-pointer words (`bind_aot_dispatch` does `*dispatch`),
        // so it must be word-aligned — without this Cranelift defaults to align-1 and the linker may
        // place `noeta_aot_dispatch` at a non-8-aligned address, which trips Rust's misaligned-pointer
        // debug check the moment the AOT runtime dereferences it.
        data.set_align(w as u64);
        data.define(bytes.into_boxed_slice());
        for p in 0..n {
            if let Some(id) = main_ids[p] {
                let fref = self.module.declare_func_in_data(id, &mut data);
                data.write_function_addr((w * (1 + 2 * p)) as u32, fref);
            }
            if let Some(id) = fast_ids[p] {
                let fref = self.module.declare_func_in_data(id, &mut data);
                data.write_function_addr((w * (1 + 2 * p + 1)) as u32, fref);
            }
        }
        let data_id = self
            .module
            .declare_data(AOT_DISPATCH_SYMBOL, Linkage::Export, false, false)
            .map_err(|e| e.to_string())?;
        self.module
            .define_data(data_id, &data)
            .map_err(|e| e.to_string())
    }

    /// Consume the compiler and emit the finished object-file bytes (ELF/Mach-O/COFF for the host).
    pub fn finish(self) -> Result<Vec<u8>, String> {
        self.module.finish().emit().map_err(|e| e.to_string())
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
fn is_eligible(chunk: &noeta_bytecode::Chunk, names: &[String]) -> bool {
    chunk.code.iter().any(|op| is_fast_op(op, names))
}

/// Whether [`emit_op`] compiles this op instance to native code (vs bailing to the interpreter at it).
/// Every `LoadConst` is fast: an immediate constant (int in the 48-bit range, bool, unit, a float)
/// materializes inline, and a heap constant (a string, a native module/fn, a method handle, a big
/// int) goes through the leaf-op helper's one shared `materialize` — a `Binary` only for the integer/
/// float arithmetic-and-comparison set.
///
/// `names` is the module's interned name table: `Op::CallMethod` is native only for a **map**
/// method name (the leaf helper handles exactly the map receiver and bails on everything else), so
/// classifying it needs to resolve its `NameId`. Restricting it statically matters — an
/// always-native `CallMethod` would make an object-method loop *look* tier-1-sustainable to
/// [`worth_osr`] and then bail out of native code on every iteration, which is slower than
/// interpreting it.
fn is_fast_op(op: &Op, names: &[String]) -> bool {
    match op {
        Op::LoadConst { .. } => true,
        Op::Move { .. } | Op::Drop { .. } => true,
        Op::Binary { op, .. } => supported_binary(*op),
        // S1 (Tier W): the sign-dependent fixed-width ops and the width wrap. Only the ops the
        // emitter handles; anything else in the field (defensive) stays a bail op.
        Op::WideInt { op, .. } => matches!(
            op,
            BinaryOp::Div
                | BinaryOp::Rem
                | BinaryOp::Shr
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
        ),
        Op::MaskWidth { .. } => true,
        Op::LoadGlobal { .. } | Op::StoreGlobal { .. } | Op::TakeGlobal { .. } => true,
        Op::Call { .. } | Op::CallGlobal { .. } | Op::Return { .. } => true,
        Op::Jump { .. }
        | Op::JumpIfTrue { .. }
        | Op::JumpIfFalse { .. }
        | Op::CondBranch { .. } => true,
        // The map fast path (`m[k]`/`m[k] = v`/`m.get_or(k, d)`): the helper serves a map receiver
        // and bails on any other, so a same-named user method costs a bail — hence the static name
        // filter, which keeps an object-method loop out of tier 1 entirely.
        Op::CallMethod { method, .. } => names
            .get(method.0 as usize)
            .is_some_and(|n| noeta_ext_abi::MapMethod::from_name(n).is_some()),
        op if is_leaf_heap_op(op) => true,
        _ => false,
    }
}

/// The leaf heap/collection ops the JIT runs through the `run_leaf_op` helper (J4) — non-dispatching
/// ops whose exact interpreter logic (refcounts included) the helper reproduces, bailing on the
/// dispatch/error cases. A prototype containing one is `heap_aware` (they can put a heap value in a
/// register).
///
/// `Op::CallMethod` is deliberately **not** here: it is a leaf op only for the map-method names
/// [`is_fast_op`] admits, and `heap_aware` accounts for it separately ([`writes_heap_reg`]).
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
            // The string-building family (all three of `"a${x}b"`'s ops, plus the `s = s ~ x`
            // accumulator): pure computation over registers and constants with no dispatch and no
            // failure path, so the helper reproduces the interpreter arm exactly and never bails.
            // `Stringify` is the one exception — an object/enum receiver may light up `Display`,
            // whose `to_string` runs bytecode — and bails there.
            | Op::Stringify { .. }
            | Op::BuildString { .. }
            | Op::ConcatInPlace { .. }
    )
}

/// Whether *native* execution of this op can leave a heap value in a register — the `heap_aware`
/// question. Every leaf heap op qualifies, as does an admitted map `CallMethod` (its result, and
/// the map it moves into the destination) and a `LoadConst` of a heap constant (which, unlike the
/// immediate form, allocates).
fn writes_heap_reg(op: &Op, consts: &[Const]) -> bool {
    match op {
        Op::LoadConst { k, .. } => const_immediate_bits(&consts[*k as usize]).is_none(),
        Op::CallMethod { .. } => true,
        op => is_leaf_heap_op(op),
    }
}

/// The binary operators the JIT compiles natively: integer arithmetic, comparison, and the P-BITS
/// Tier-B bitwise/shift family (S1 — int-only: a non-int operand pairing bails, the interpreter
/// raises its E0043). (`~`/identity/logical are not integer ops; the fixed-width `WideInt` op has
/// its own emit arm.)
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
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr
    )
}

/// The Tier-B bitwise/shift subset of [`supported_binary`] — **integer-only** ops: the emitter
/// skips the float paths entirely (a non-int operand bails; the interpreter raises E0043), and the
/// kind map may claim their natively-stored destination `Int` unconditionally.
fn bitwise_binary(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr
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

/// The **OSR window** for a back-edge-born compile that entered at `header`: the pc interval a
/// region-scoped body compiles natively, everything outside it a bail (P-OSRW).
///
/// The window is the union of every loop whose `[header, back_edge]` extent overlaps it, grown to
/// a fixpoint. Seeded at `[header, header]`, that absorbs the loop `header` heads *and every loop
/// enclosing it* — which is the point: OSR fires at whichever back-edge got hot first, usually the
/// **innermost** loop of a nest, and a window covering only that loop would bail out of native code
/// on every outer iteration (the interpreter re-enters native only at a frame `'reload`, and a
/// call-free outer loop has none). Sibling loops before or after the window do not overlap it and
/// stay out. Because every absorbed extent contains `header`, the union is always an interval.
///
/// Extents are the same `[header, back_edge]` over-approximation [`worth_osr`] scans with, so a
/// window is native-sustainable exactly where that gate says the loop is.
///
/// `None` when `header` is not a loop header (no back edge targets it) — there is no window to
/// scope to.
fn osr_region(chunk: &noeta_bytecode::Chunk, header: usize) -> Option<(usize, usize)> {
    let extents: Vec<(usize, usize)> = chunk
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, op)| backward_target(op, pc).map(|h| (h, pc)))
        .collect();
    if !extents.iter().any(|&(h, _)| h == header) {
        return None;
    }
    let (mut lo, mut hi) = (header, header);
    loop {
        let mut grew = false;
        for &(h, b) in &extents {
            // Overlap test on closed intervals; absorb the whole extent when it touches the window.
            if h <= hi && b >= lo {
                if h < lo {
                    lo = h;
                    grew = true;
                }
                if b > hi {
                    hi = b;
                    grew = true;
                }
            }
        }
        if !grew {
            return Some((lo, hi));
        }
    }
}

/// Whether OSR-compiling this prototype is worthwhile: it has at least one loop whose body native
/// code can **sustain** — every op between a loop header and its back-edge compiles to native code
/// ([`is_fast_op`]). A loop whose body contains a bail op exits native on the first such op *every*
/// iteration (a tier-0↔tier-1 bounce that costs more than just interpreting the loop), so a prototype
/// whose only loops bail is left in the interpreter. The loop body is over-approximated as the pc
/// range `[header, back_edge]` — conservative (an occasional bail in a rarely-taken branch declines a
/// loop that is mostly native-able), which errs toward not regressing a heap-op-dominated loop.
pub fn worth_osr(chunk: &noeta_bytecode::Chunk, names: &[String]) -> bool {
    let code = &chunk.code;
    code.iter().enumerate().any(|(pc, op)| {
        backward_target(op, pc)
            .is_some_and(|header| code[header..=pc].iter().all(|o| is_fast_op(o, names)))
    })
}

/// The coverage-gap sites behind a [`worth_osr`] decline: every pc inside a loop body whose op is
/// not native ([`is_fast_op`]), for each loop (over-approximated as `[header, back_edge]`, exactly as
/// `worth_osr` scans). These are the ops that keep a hot loop off tier 1 — the JIT bail report
/// (`noeta run --jit-stats`) names them so "what should become JITable next" is a measurement, not a
/// guess. Deduplicated and sorted; empty when every loop is native-sustainable (or there is no loop).
pub fn loop_bail_pcs(chunk: &noeta_bytecode::Chunk, names: &[String]) -> Vec<usize> {
    let code = &chunk.code;
    let mut pcs: Vec<usize> = code
        .iter()
        .enumerate()
        .filter_map(|(pc, op)| backward_target(op, pc).map(|header| (header, pc)))
        .flat_map(|(header, back_edge)| {
            code[header..=back_edge]
                .iter()
                .enumerate()
                .filter(|(_, o)| !is_fast_op(o, names))
                .map(move |(i, _)| header + i)
                .collect::<Vec<_>>()
        })
        .collect();
    pcs.sort_unstable();
    pcs.dedup();
    pcs
}

/// Whether compiling this prototype is worthwhile at all (the entry path *and* OSR). A loopless
/// prototype — a recursive function like `fib`, straight-line code — is worth compiling: it runs its
/// body once per activation with no per-iteration bail bounce. A prototype *with* a loop is worth it
/// only if some loop is native-sustainable ([`worth_osr`]); one whose every loop bails would bounce
/// tier-0↔tier-1 every iteration, slower than just interpreting it.
pub fn worth_compiling(chunk: &noeta_bytecode::Chunk, names: &[String]) -> bool {
    !has_osr_entry(chunk) || worth_osr(chunk, names)
}

/// Forward reachability of each bytecode pc in the *native* control-flow graph, seeded from every
/// native entry point ([`entry_pcs`]) — a fresh frame (pc 0) and every post-call resume pc. A non-fast
/// op is terminal — it bails (returns its pc), so it has no native successor — which is why this
/// follows edges only out of fast ops. Used so the codegen fills unreachable blocks (dead code, or the
/// fall-through past a bail) with a trivial bail instead of code that would reference the entry-only
/// frame/globals pointers from a non-dominated block.
///
/// **A prototype's native region is not free to grow (measured).** Nativizing one more op widens
/// this set, and a *whole-function* Cranelift compile then allocates registers across the larger
/// body — which can cost an unrelated hot loop in the *same* prototype. Concretely: making the
/// trailing `Stringify` of `echo sum` compilable pushed the `Echo`/`Halt` tail into the reachable
/// set of a top-level script whose hot loop is `while i < 2000 { sum = sum + xs[i] }`, and the loop
/// slowed ~5% (a register that had lived in `%r14` started spilling to the stack every iteration).
/// The same program with the loop moved into its own `fn` is unaffected — the small prototype's
/// allocation does not change.
///
/// That is what the **region-scoped OSR body** ([`Jit::compile_osr`], [`osr_region`]) exists to
/// fix: a back-edge-born compile emits a second body whose native region is the loop's own pc
/// window, so Cranelift allocates registers for the loop rather than for the union of the loop and
/// a cold tail the OSR entry will never reach. The whole-prototype body still serves the pc-0 and
/// post-call entries, so nothing loses coverage.
///
/// `entries` seeds the walk (the whole-prototype body's is [`entry_pcs`]; a fast body's is just
/// `{0}`; a region body's is [`entry_pcs`] confined to its window). `region`, when set, is the
/// closed pc interval a region-scoped body compiles: a pc outside it is terminal exactly as a
/// non-fast op is — native code bails there, so its block gets the ordinary sync-and-return-pc
/// treatment and nothing beyond it is emitted.
fn reachable_pcs_from(
    chunk: &noeta_bytecode::Chunk,
    entries: Vec<usize>,
    names: &[String],
    region: Option<(usize, usize)>,
) -> Vec<bool> {
    let n = chunk.code.len();
    let mut seen = vec![false; n];
    let mut stack = entries;
    while let Some(pc) = stack.pop() {
        if pc >= n || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let op = &chunk.code[pc];
        if !in_region(region, pc) || !is_fast_op(op, names) {
            continue; // a bail point (or the region's edge): no native successor
        }
        match op {
            // A native `Call`/`CallGlobal` continues at `pc + 1` on the direct/fast path
            // (J3/S4.1) — this edge is what lets a fast body run its post-call ops natively
            // instead of compiling them to bail fillers. `Return` ends the frame.
            Op::Call { .. } | Op::CallGlobal { .. } => stack.push(pc + 1),
            Op::Return { .. } => {}
            // Branch destinations come from [`Op::for_each_jump_pc`] rather than a list repeated
            // here; `Jump` is the only branch that does not also fall through. Today `is_fast_op`
            // admits no other destination-carrying op, so this is exactly the previous edge set —
            // and it stays exact if the whitelist grows one.
            _ => {
                op.for_each_jump_pc(|t| stack.push(t as usize));
                if !matches!(op, Op::Jump { .. }) {
                    stack.push(pc + 1); // fast straight-line op, or a branch's fall-through
                }
            }
        }
    }
    seen
}

/// Whether `pc` is inside a region-scoped body's native window. `None` — a whole-prototype body —
/// admits every pc.
fn in_region(region: Option<(usize, usize)>, pc: usize) -> bool {
    match region {
        None => true,
        Some((lo, hi)) => pc >= lo && pc <= hi,
    }
}

/// P-JCT C3: the fixed NaN-box/outcome constants every body needs, materialized **once in the
/// entry block** (which dominates every reachable block) instead of re-emitted at each use —
/// duplicate `iconst`s were roughly a third of the IR handed to Cranelift, and compile time is
/// proportional to what we hand it, not to what survives GVN. Unreachable-pc blocks (no
/// predecessors, so not dominated by entry) must not read the pool — they emit their two
/// constants locally ([`Codegen::ret_bail_isolated`]).
#[derive(Clone, Copy)]
struct ConstPool {
    zero: ClValue,
    unit: ClValue,
    /// `qnan` — the [`Codegen::is_float`] mask/want.
    qnan: ClValue,
    /// `sign | qnan` — the [`Codegen::is_pointer`] mask/want.
    ptr_tag: ClValue,
    /// `sign | qnan | int_tag` — the [`Codegen::is_small_int`] mask.
    int_mask: ClValue,
    /// `qnan | int_tag` — the [`Codegen::is_small_int`] want and the small-int retag word.
    int_tag: ClValue,
    /// The low-48-bit payload mask.
    ptr_mask: ClValue,
    true_bits: ClValue,
    false_bits: ClValue,
    /// `Value::float(f64::NAN).bits()` — the canonical quiet NaN a float op's NaN result becomes.
    nan_canon: ClValue,
    unbound: ClValue,
    outcome_returned: ClValue,
    outcome_continue: ClValue,
}

impl ConstPool {
    /// Emit the pool into the current (entry) block.
    fn seed(b: &mut FunctionBuilder) -> ConstPool {
        let l = Value::NANBOX;
        let mut k = |v: i64| b.ins().iconst(types::I64, v);
        ConstPool {
            zero: k(0),
            unit: k(l.unit_bits as i64),
            qnan: k(l.qnan as i64),
            ptr_tag: k((l.sign_bit | l.qnan) as i64),
            int_mask: k((l.sign_bit | l.qnan | l.int_tag) as i64),
            int_tag: k((l.qnan | l.int_tag) as i64),
            ptr_mask: k(l.ptr_mask as i64),
            true_bits: k(l.true_bits as i64),
            false_bits: k(l.false_bits as i64),
            nan_canon: k(Value::float(f64::NAN).bits() as i64),
            unbound: k(l.unbound_bits as i64),
            outcome_returned: k(OUTCOME_RETURNED),
            outcome_continue: k(OUTCOME_CONTINUE),
        }
    }
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
    /// The entry-block constant pool (P-JCT C3) — see [`ConstPool`].
    pool: ConstPool,
    /// P-JCT C3: one shared bail block per pc, created and filled (frame sync + bail return) on
    /// the first [`guard`] at that pc; later guards of the same op reuse it. An op like
    /// `a = a * 3 + 1` carries several guards (operand tags, overflow fits), and each bail body
    /// is a full [`Codegen::sync_frame`] — per-guard bail blocks were a large slice of the IR
    /// volume. Sound because the spill *set* is pc-keyed (identical for every guard of the pc)
    /// and the spilled *values* resolve per-predecessor through the SSA builder's block params.
    bail_blocks: Vec<Option<Block>>,
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
    /// The run's cancellation-flag address, or `0` for "this run cannot be cancelled" — see
    /// [`Jit::cancel_flag`]. Nonzero makes [`emit_cancel_poll`] fire at every loop header;
    /// zero emits nothing, which is every ordinary run.
    cancel_addr: u64,
    /// Per-pc: is this pc a **loop header** (the target of some backward branch, i.e. an OSR
    /// entry — [`backward_target`])? The cancellation poll's placement, computed once per body.
    /// Empty when `cancel_addr == 0`, so a non-cancellable compile does not even build it.
    loop_header: Vec<bool>,
    /// P-AOT L3.1: emit for an ahead-of-time object (`true`) instead of the runtime JIT (`false`).
    /// The only codegen difference is at call sites: the JIT bakes each site's inline-cache slot as
    /// an absolute address (`site_addrs`), which is meaningless in a relocatable object. An AOT body
    /// therefore emits **no** inline-cache hit path and passes a null slot to `prepare_call` — the
    /// always-correct helper slow path (calls just skip the per-site cache). Everything else — the
    /// frame-template copy (position-independent immediate words) and helper calls (`Linkage::Import`
    /// relocations) — is already object-safe, so this is the whole seam.
    aot: bool,
    note_bound_ref: FuncRef,
    retain_ref: FuncRef,
    release_ref: FuncRef,
    release_value_ref: FuncRef,
    call_ref: FuncRef,
    return_ref: FuncRef,
    prepare_call_ref: FuncRef,
    after_call_ref: FuncRef,
    leaf_op_ref: FuncRef,
    fmod_ref: FuncRef,
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
        let mut any: Option<ClValue> = None;
        for r in 0..self.nreg as u16 {
            if self.heap_in[pc * self.nreg + r as usize] || self.const_bits[r as usize].is_some() {
                continue;
            }
            let v = self.load_reg(r);
            let viol = match self.kind_claim(pc, r) {
                Kind::Int => {
                    // Violated unless the small-int tag matches.
                    let masked = self.b.ins().band(v, self.pool.int_mask);
                    self.b
                        .ins()
                        .icmp(IntCC::NotEqual, masked, self.pool.int_tag)
                }
                Kind::Bool => {
                    // Violated unless the word is exactly `true` or `false`.
                    let is_t = self.b.ins().icmp(IntCC::Equal, v, self.pool.true_bits);
                    let is_f = self.b.ins().icmp(IntCC::Equal, v, self.pool.false_bits);
                    let is_bool = self.b.ins().bor(is_t, is_f);
                    self.b.ins().bxor_imm(is_bool, 1)
                }
                Kind::Float => {
                    // Violated if the word is qnan-tagged (every non-f64 value is).
                    let masked = self.b.ins().band(v, self.pool.qnan);
                    self.b.ins().icmp(IntCC::Equal, masked, self.pool.qnan)
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
                    let is_t = self.b.ins().icmp(IntCC::Equal, v, self.pool.true_bits);
                    self.b.ins().uextend(types::I64, is_t)
                }
                _ => self.pool.zero,
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
        let unit = self.pool.unit;
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
            self.b.ins().return_(&[outcome, self.pool.zero]);
        } else {
            self.b.ins().return_(&[outcome]);
        }
    }

    /// [`Codegen::ret_bail`] for a block with **no predecessors** (an unreachable pc's block):
    /// such a block is not dominated by the entry block, so it must not read the constant pool —
    /// both words are emitted locally.
    fn ret_bail_isolated(&mut self, pc: usize) {
        let here = self.b.ins().iconst(types::I64, pc as i64);
        if self.fast {
            let zero = self.b.ins().iconst(types::I64, 0);
            self.b.ins().return_(&[here, zero]);
        } else {
            self.b.ins().return_(&[here]);
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
        let masked = self.b.ins().band(v, self.pool.ptr_tag);
        self.b.ins().icmp(IntCC::Equal, masked, self.pool.ptr_tag)
    }

    /// `(v & (sign|qnan|int_tag)) == (qnan|int_tag)` — is `v` an immediate small int?
    fn is_small_int(&mut self, v: ClValue) -> ClValue {
        let masked = self.b.ins().band(v, self.pool.int_mask);
        self.b.ins().icmp(IntCC::Equal, masked, self.pool.int_tag)
    }

    /// Unbox a small-int word to its i64: sign-extend the low 48-bit payload (`(v << 16) >> 16` —
    /// the shift pair discards the tag bits itself, no payload mask needed).
    fn unbox_int(&mut self, v: ClValue) -> ClValue {
        let shl = self.b.ins().ishl_imm(v, 16);
        self.b.ins().sshr_imm(shl, 16)
    }

    /// `(v & qnan) != qnan` — is `v` an f64 float? (Every tagged value — int/bool/unit/f32/pointer —
    /// has all the qnan bits set; a float is exactly the words that don't.)
    fn is_float(&mut self, v: ClValue) -> ClValue {
        let masked = self.b.ins().band(v, self.pool.qnan);
        self.b.ins().icmp(IntCC::NotEqual, masked, self.pool.qnan)
    }

    /// Reinterpret a float word's bits as the f64 it stores (the value is known to be a float — a
    /// float is stored as its own bit pattern, not tagged).
    fn bits_to_f64(&mut self, v: ClValue) -> ClValue {
        self.b.ins().bitcast(types::F64, MemFlagsData::new(), v)
    }

    /// `v == unbound` — is `v` the VM's unbound-global sentinel?
    fn is_unbound(&mut self, v: ClValue) -> ClValue {
        self.b.ins().icmp(IntCC::Equal, v, self.pool.unbound)
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
fn emit_op(
    cg: &mut Codegen,
    consts: &[Const],
    names: &[String],
    op: &Op,
    pc: usize,
    op_blocks: &[Block],
) {
    cg.cur_pc = pc;
    if !is_fast_op(op, names) {
        cg.sync_frame(pc);
        let here = cg.pc_const(pc);
        cg.ret_bail(here);
        return;
    }
    let next = |cg: &mut Codegen| cg.b.ins().jump(op_blocks[pc + 1], &[]);
    match op {
        Op::LoadConst { dst, k } => {
            let c = &consts[*k as usize];
            // A heap constant allocates, so it has no inline bit pattern — route it through the
            // leaf-op helper's shared `materialize` instead of materializing it here.
            let Some(bits) = const_immediate_bits(c) else {
                return emit_leaf_op(cg, pc, op_blocks);
            };
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
            let unit = cg.pool.unit;
            cg.store_reg_raw(*reg, unit);
            if !cg.transfer[pc] && cg.may_be_heap(*reg) {
                cg.release_dropped_if_heap(v, *relevant);
            }
            next(cg);
        }
        Op::Binary { op, dst, a, b, .. } => emit_binary(cg, *op, *dst, *a, *b, pc, op_blocks),
        Op::WideInt {
            op,
            dst,
            a,
            b,
            signed,
            bits,
            ..
        } => emit_wide_int(cg, *op, *dst, *a, *b, *signed, *bits, pc, op_blocks),
        Op::MaskWidth {
            dst,
            src,
            signed,
            bits,
        } => emit_mask_width(cg, *dst, *src, *signed, *bits, pc, op_blocks),
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
            let is_true = cg.b.ins().icmp(IntCC::Equal, v, cg.pool.true_bits);
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
            let is_false = cg.b.ins().icmp(IntCC::Equal, v, cg.pool.false_bits);
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
            let fb = cg.pool.false_bits;
            let tb = cg.pool.true_bits;
            // Both continuations proved the scrutinee a bool, and the kind map claims it Bool
            // downstream (P-JCT C3 guard strengthening) — define its raw 0/1 form here, where it
            // dominates both successors (`true` reaches the fallthrough only, so the bit is
            // exact on every path that reads it).
            let is_t = cg.b.ins().icmp(IntCC::Equal, v, tb);
            let raw = cg.b.ins().uextend(types::I64, is_t);
            cg.def_raw(*reg, raw);
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
        // Admitted by `is_fast_op` only for a map-method name — the helper serves the map receiver
        // and bails on anything else (including a same-named user method).
        Op::CallMethod { .. } => emit_leaf_op(cg, pc, op_blocks),
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
    // In an AOT body the per-site inline-cache slot is a per-process heap address that cannot be
    // baked into a relocatable object (P-AOT L3.1), so `site` is null and the inline-cache hit path
    // below is not emitted at all — calls go straight to the always-correct `prepare_call` helper,
    // which treats a null slot as "no site cache".
    let site = if cg.aot {
        cg.b.ins().iconst(types::I64, 0)
    } else {
        let site_addr = cg.site_addrs[pc];
        cg.b.ins().iconst(types::I64, site_addr as i64)
    };
    if !cg.aot {
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
            std::slice::from_raw_parts(cg.frame_template_addr as *const u64, l.frame_size / 8)
                .to_vec()
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
    } else {
        // AOT: no inline cache — jump straight to the helper slow path.
        cg.b.ins().jump(slow_blk, &[]);
    }

    // ---- Slow path: the prepare helper (which also fills or poisons the site cache). ----
    cg.b.switch_to_block(slow_blk);
    let proto = cg.b.ins().iconst(types::I32, cg.proto as i64);
    let pcv = cg.b.ins().iconst(types::I32, pc as i64);

    // Try a direct native→native call: `prepare_call` returns the callee's compiled entry pointer
    // and its reserved window base in one roundtrip (S4.0), or a zero pointer if the call is not
    // direct-able (uncompiled callee, defaults/upvalues, no stack capacity). A pointer carrying
    // [`noeta_jit_abi::FAST_ENTRY_TAG`] is a **fast-convention** entry (S4.1): its window was
    // reserved uninitialized and the arguments travel as machine arguments. The tag is only
    // readable because every body is [`noeta_jit_abi::MIN_BODY_ALIGNMENT`]-aligned (`make_isa`);
    // `jit_install` refuses any entry that is not, so an untagged pointer here is always an address.
    let prep = cg.prepare_call_ref;
    let pinst =
        cg.b.ins()
            .call(prep, &[vm, frames, regs_vec, base, proto, pcv, site]);
    let fnptr = cg.b.inst_results(pinst)[0];
    let callee_base = cg.b.inst_results(pinst)[1];
    let fast_bit =
        cg.b.ins()
            .band_imm(fnptr, noeta_jit_abi::FAST_ENTRY_TAG as i64);
    let tagged_blk = cg.b.create_block();
    let untagged = cg.b.create_block();
    cg.b.ins().brif(fast_bit, tagged_blk, &[], untagged, &[]);
    cg.b.switch_to_block(tagged_blk);
    let fp_untagged =
        cg.b.ins()
            .band_imm(fnptr, !(noeta_jit_abi::FAST_ENTRY_TAG as i64));
    cg.b.ins()
        .jump(fast_blk, &[fp_untagged.into(), callee_base.into()]);

    cg.b.switch_to_block(untagged);
    let is_zero = cg.b.ins().icmp(IntCC::Equal, fnptr, cg.pool.zero);
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
    let entry0 = cg.pool.zero;
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
    let returned = cg.pool.outcome_returned;
    let is_returned = cg.b.ins().icmp(IntCC::Equal, callee_outcome, returned);
    let cold_blk = cg.b.create_block();
    cg.b.ins()
        .brif(is_returned, continue_blk, &[], cold_blk, &[]);
    cg.b.switch_to_block(cold_blk);
    let ainst =
        cg.b.ins()
            .call(cg.after_call_ref, &[vm, frames, callee_outcome]);
    let after = cg.b.inst_results(ainst)[0];
    let cont = cg.pool.outcome_continue;
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
    let f_returned = cg.pool.outcome_returned;
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
    let f_cont = cg.pool.outcome_continue;
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
    let cont = cg.pool.outcome_continue;
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
    let outcome = cg.pool.outcome_returned;
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
/// The source's reference transfers into the global, so there is never a retain. The old occupant
/// decides the tail: unbound → the helper records the first binding in `global_order`; otherwise the
/// displaced value is released, destructor-aware, exactly as the interpreter's `release_value`.
///
/// A displaced **heap** value is native only in a `heap_aware` body — its release may run a
/// `destruct` block, which is the same re-entrant call an IR-relevant `Op::Drop` already emits
/// there. Elsewhere it bails (the immediate invariant makes the release a provable no-op, so the
/// non-heap-aware body simply never needs the call).
fn emit_store_global(cg: &mut Codegen, g: u32, src: Reg, pc: usize, op_blocks: &[Block]) {
    let old = cg.load_global(g);
    // Decide the bail BEFORE mutating anything: a bail hands control back to the interpreter, which
    // re-runs this op, so no register or slot may have changed yet. `is_pointer` is false for the
    // unbound sentinel, so this never catches the first-bind case.
    if !cg.heap_aware {
        let heap = cg.is_pointer(old);
        let cont = cg.b.create_block();
        let bail = cg.b.create_block();
        cg.b.ins().brif(heap, bail, &[], cont, &[]);
        cg.b.switch_to_block(bail);
        cg.sync_frame(pc);
        let here = cg.pc_const(pc);
        cg.ret_bail(here);
        cg.b.switch_to_block(cont);
    }

    // Safe to mutate. Take the source out (moved into the global — no release, its reference
    // transfers) and write the slot; a first bind records it in `global_order`, a rebind releases
    // the value it displaced.
    let v = cg.read_reg(src);
    cg.store_reg_raw(src, cg.pool.unit);
    cg.store_global(g, v);
    let is_unb = cg.is_unbound(old);
    let bind_blk = cg.b.create_block();
    let rebind_blk = cg.b.create_block();
    let after = cg.b.create_block();
    cg.b.ins().brif(is_unb, bind_blk, &[], rebind_blk, &[]);
    cg.b.switch_to_block(bind_blk);
    let vm = cg.vm;
    let gid = cg.b.ins().iconst(types::I32, g as i64);
    let note = cg.note_bound_ref;
    cg.b.ins().call(note, &[vm, gid]);
    cg.b.ins().jump(after, &[]);
    cg.b.switch_to_block(rebind_blk);
    if cg.heap_aware {
        // The displaced value's destructor fires here if this was its last reference — the
        // interpreter's `release_value`, through the same helper `Op::Drop` uses.
        cg.release_dropped_if_heap(old, true);
    }
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
    // A heap value *moves* out of the slot into `dst` — the single owning reference transfers, so
    // there is no retain to emit; the only refcount work is releasing whatever `dst` held, which is
    // exactly what a `heap_aware` `store_reg` does (the interpreter's `mem::replace` + `set_reg`).
    // Outside a `heap_aware` body the immediate invariant forbids a heap value in a register at
    // all, so the move stays a bail there. This matters more than it looks: a top-level `mut`
    // accumulator (a map, a string) lives in a global, so `m[k] = v` / `s = s ~ x` in a loop reads
    // it through `TakeGlobal` every iteration — bailing here bailed the whole loop out of tier 1
    // on its *first* body op, however native the rest of it was.
    let bail_cond = if cg.heap_aware {
        unbound
    } else {
        let heap = cg.is_pointer(old);
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
    cg.store_global(g, cg.pool.unit);
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
    if ka == Kind::Float && kb == Kind::Float && !bitwise_binary(op) {
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
        // The guard proves the unknown side `Int` on every native continuation, and the kind
        // map claims it downstream (P-JCT C3 guard strengthening) — so its raw variable must be
        // made current here, exactly like a typed def's.
        let (x, y) = if ka == Kind::Int {
            let x = cg.read_raw_int(a);
            let vb = cg.read_reg(b);
            let ok = cg.is_small_int(vb);
            let cont = cg.b.create_block();
            guard(cg, ok, cont, pc);
            let y = cg.unbox_int(vb);
            cg.def_raw(b, y);
            (x, y)
        } else {
            let va = cg.read_reg(a);
            let ok = cg.is_small_int(va);
            let cont = cg.b.create_block();
            guard(cg, ok, cont, pc);
            let y = cg.read_raw_int(b);
            let x = cg.unbox_int(va);
            cg.def_raw(a, x);
            (x, y)
        };
        emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);
        return;
    }
    if ((ka == Kind::Float && kb == Kind::Imm) || (ka == Kind::Imm && kb == Kind::Float))
        && !bitwise_binary(op)
    {
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

    // Tier-B bitwise (S1) is integer-only — no float body exists, so the dispatch is
    // int-or-bail: a non-int pairing (boxed big int, float, `dyn` misuse) is the
    // interpreter's to handle (it raises E0043 on a genuine type error).
    if bitwise_binary(op) {
        let int_block = cg.b.create_block();
        guard(cg, both_int, int_block, pc);
        let x = cg.unbox_int(va);
        let y = cg.unbox_int(vb);
        emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);
        return;
    }

    let int_block = cg.b.create_block();
    let float_check = cg.b.create_block();
    cg.b.ins().brif(both_int, int_block, &[], float_check, &[]);

    // Integer fast path.
    cg.b.switch_to_block(int_block);
    let x = cg.unbox_int(va);
    let y = cg.unbox_int(vb);
    emit_int_binary_raw(cg, op, dst, x, y, pc, op_blocks);

    // Float fast path: both operands f64 floats.
    cg.b.switch_to_block(float_check);
    let a_flt = cg.is_float(va);
    let b_flt = cg.is_float(vb);
    let both_flt = cg.b.ins().band(a_flt, b_flt);
    let float_block = cg.b.create_block();
    let mixed_check = cg.b.create_block();
    cg.b.ins()
        .brif(both_flt, float_block, &[], mixed_check, &[]);

    cg.b.switch_to_block(float_block);
    emit_float_binary(cg, op, dst, va, vb, pc, op_blocks);

    // Mixed int/f64 lane (S2, or bail): exactly one side a small int — widen it to f64 (an
    // immediate is ≤48 bits, exactly representable, matching the interpreter's `as f64`) and run
    // the float body. Any other pairing (f32, boxed, non-numeric) bails. Branchless conversion:
    // compute both readings per side and select on the int flag — the discarded reading is
    // garbage bits, never a trap.
    cg.b.switch_to_block(mixed_check);
    let a_mixed = cg.b.ins().band(a_int, b_flt);
    let b_mixed = cg.b.ins().band(a_flt, b_int);
    let mixed = cg.b.ins().bor(a_mixed, b_mixed);
    let mixed_block = cg.b.create_block();
    guard(cg, mixed, mixed_block, pc);
    let a_raw = cg.unbox_int(va);
    let a_widened = cg.b.ins().fcvt_from_sint(types::F64, a_raw);
    let a_as_f64 = cg.bits_to_f64(va);
    let x = cg.b.ins().select(a_int, a_widened, a_as_f64);
    let b_raw = cg.unbox_int(vb);
    let b_widened = cg.b.ins().fcvt_from_sint(types::F64, b_raw);
    let b_as_f64 = cg.bits_to_f64(vb);
    let y = cg.b.ins().select(b_int, b_widened, b_as_f64);
    emit_float_binary_vals(cg, op, dst, x, y, pc, op_blocks);
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
            let nonzero = cg.b.ins().icmp(IntCC::NotEqual, y, cg.pool.zero);
            let ok = cg.b.create_block();
            guard(cg, nonzero, ok, pc);
            let r = if op == BinaryOp::Div {
                cg.b.ins().sdiv(x, y)
            } else {
                cg.b.ins().srem(x, y)
            };
            box_int_and_store(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
            // `& | ^` of two sign-extended 48-bit immediates is itself sign-extended 48-bit
            // (bitwise ops commute with sign extension), so the fit check is provably true —
            // store without it.
            let r = match op {
                BinaryOp::BitAnd => cg.b.ins().band(x, y),
                BinaryOp::BitOr => cg.b.ins().bor(x, y),
                _ => cg.b.ins().bxor(x, y),
            };
            store_int_unchecked(cg, dst, r, pc, op_blocks);
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // The interpreter raises on a shift amount outside `0..64` — bail before any write
            // (`(y as u64) < 64` covers both the negative and the ≥64 case in one unsigned test).
            let in_range = cg.b.ins().icmp_imm(IntCC::UnsignedLessThan, y, 64);
            let ok = cg.b.create_block();
            guard(cg, in_range, ok, pc);
            if op == BinaryOp::Shl {
                // i64 `<<` with the interpreter's wrapping semantics; a result past the 48-bit
                // immediate range bails (the interpreter heap-boxes it), like `Add`'s overflow.
                let r = cg.b.ins().ishl(x, y);
                box_int_and_store(cg, dst, r, pc, op_blocks);
            } else {
                // Arithmetic (sign-filling) shift: a 48-bit sign-extended value stays 48-bit
                // sign-extended under `>>`, so no fit check.
                let r = cg.b.ins().sshr(x, y);
                store_int_unchecked(cg, dst, r, pc, op_blocks);
            }
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
            let boxed =
                cg.b.ins()
                    .select(cmp, cg.pool.true_bits, cg.pool.false_bits);
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
    let shl = cg.b.ins().ishl_imm(r, 16);
    let ext = cg.b.ins().sshr_imm(shl, 16);
    let fits = cg.b.ins().icmp(IntCC::Equal, ext, r);

    // Guard: result fits the 48-bit immediate range, else bail (a big int must heap-box).
    let store = cg.b.create_block();
    guard(cg, fits, store, pc);
    store_int_unchecked(cg, dst, r, pc, op_blocks);
}

/// Store an i64 result that is **provably** in the 48-bit immediate range (a bitwise `& | ^`/`>>`
/// of immediates, or a value that already passed [`box_int_and_store`]'s fit guard): box the low
/// 48 bits with the int tag, keep the raw form current (T1), and continue.
fn store_int_unchecked(cg: &mut Codegen, dst: Reg, r: ClValue, pc: usize, op_blocks: &[Block]) {
    let lo = cg.b.ins().band(r, cg.pool.ptr_mask);
    cg.def_raw(dst, r);
    let boxed = cg.b.ins().bor(lo, cg.pool.int_tag);
    cg.store_reg(dst, boxed);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// `Op::WideInt` (Tier W3, S1): the sign-dependent fixed-width ops `/ % >> < <= > >=` on
/// erased-i64 operands read as `signed`/unsigned `bits`-wide. Operand dispatch is int-or-bail
/// (kind claims skip the guard); `/ %` guard the zero divisor (tier 0 raises E0008), mask their
/// result into the width (matching `apply_binary_wide`), and store through the fit guard (an
/// unsigned 64-bit quotient can exceed the immediate range — tier 0 heap-boxes it); `>>` guards
/// the `0..64` amount; comparisons compare the raw words with the right signedness. Every bail
/// precedes any write.
#[allow(clippy::too_many_arguments)]
fn emit_wide_int(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    a: Reg,
    b: Reg,
    signed: bool,
    bits: u8,
    pc: usize,
    op_blocks: &[Block],
) {
    // Operands: raw ints, guarded like `emit_binary`'s generic int path unless claimed.
    let ka = cg.kind_claim(pc, a);
    let kb = cg.kind_claim(pc, b);
    let (x, y) = if ka == Kind::Int && kb == Kind::Int {
        (cg.read_raw_int(a), cg.read_raw_int(b))
    } else {
        let va = cg.read_reg(a);
        let vb = cg.read_reg(b);
        let a_int = cg.is_small_int(va);
        let b_int = cg.is_small_int(vb);
        let both_int = cg.b.ins().band(a_int, b_int);
        let ok = cg.b.create_block();
        guard(cg, both_int, ok, pc);
        (cg.unbox_int(va), cg.unbox_int(vb))
    };
    match op {
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            // The erased word *is* the u64 bit pattern, so an unsigned comparison of the raw
            // i64s is exactly `a as u64 < b as u64` — no masking (values are kept in-width by
            // construction).
            let cc = match (op, signed) {
                (BinaryOp::Lt, true) => IntCC::SignedLessThan,
                (BinaryOp::Le, true) => IntCC::SignedLessThanOrEqual,
                (BinaryOp::Gt, true) => IntCC::SignedGreaterThan,
                (BinaryOp::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                (BinaryOp::Lt, false) => IntCC::UnsignedLessThan,
                (BinaryOp::Le, false) => IntCC::UnsignedLessThanOrEqual,
                (BinaryOp::Gt, false) => IntCC::UnsignedGreaterThan,
                _ => IntCC::UnsignedGreaterThanOrEqual,
            };
            let cmp = cg.b.ins().icmp(cc, x, y);
            let raw = cg.b.ins().uextend(types::I64, cmp);
            cg.def_raw(dst, raw);
            let boxed =
                cg.b.ins()
                    .select(cmp, cg.pool.true_bits, cg.pool.false_bits);
            cg.store_reg(dst, boxed);
            cg.b.ins().jump(op_blocks[pc + 1], &[]);
        }
        BinaryOp::Div | BinaryOp::Rem => {
            // Bail on a zero divisor (tier 0 raises E0008). Signed overflow (MIN / -1) cannot
            // arise: an unboxed immediate is never i64::MIN.
            let nonzero = cg.b.ins().icmp(IntCC::NotEqual, y, cg.pool.zero);
            let ok = cg.b.create_block();
            guard(cg, nonzero, ok, pc);
            let r = match (op, signed) {
                (BinaryOp::Div, true) => cg.b.ins().sdiv(x, y),
                (BinaryOp::Div, false) => cg.b.ins().udiv(x, y),
                (BinaryOp::Rem, true) => cg.b.ins().srem(x, y),
                _ => cg.b.ins().urem(x, y),
            };
            let m = emit_mask_to_width(cg, r, signed, bits);
            // The fit guard is live: an unsigned 64-bit quotient of a negative-erased word (a
            // huge u64) can exceed the immediate range — tier 0 boxes it.
            box_int_and_store(cg, dst, m, pc, op_blocks);
        }
        BinaryOp::Shr => {
            // Amount in `0..64` or tier 0 raises; one unsigned test covers negative and ≥64.
            let in_range = cg.b.ins().icmp_imm(IntCC::UnsignedLessThan, y, 64);
            let ok = cg.b.create_block();
            guard(cg, in_range, ok, pc);
            if signed {
                // Arithmetic shift keeps a sign-extended immediate sign-extended — no fit check.
                let r = cg.b.ins().sshr(x, y);
                store_int_unchecked(cg, dst, r, pc, op_blocks);
            } else {
                // Logical shift of a negative-erased word (u64 with high bits) can land above
                // the immediate range (`u64::MAX >> 1`) — the fit guard bails, tier 0 boxes.
                let r = cg.b.ins().ushr(x, y);
                box_int_and_store(cg, dst, r, pc, op_blocks);
            }
        }
        _ => unreachable!("is_fast_op gate: unexpected WideInt op {op:?}"),
    }
}

/// Mask an i64 result into a `signed`/unsigned `bits`-wide integer — `mask_to_width` in native
/// code, with `bits` a compile-time constant: ≥64 is the identity, signed sign-extends via a
/// shift pair, unsigned keeps the low bits.
fn emit_mask_to_width(cg: &mut Codegen, r: ClValue, signed: bool, bits: u8) -> ClValue {
    if bits >= 64 {
        return r;
    }
    if signed {
        let shl = cg.b.ins().ishl_imm(r, i64::from(64 - bits));
        cg.b.ins().sshr_imm(shl, i64::from(64 - bits))
    } else {
        cg.b.ins().band_imm(r, ((1u64 << bits) - 1) as i64)
    }
}

/// `Op::MaskWidth` (Tier W, S1): wrap an erased result into its fixed width. Total in tier 0; the
/// native form guards only the operand (an immediate int — a boxed word bails). The masked result
/// of an immediate is itself immediate for every emitted width (`{8,16,32}` shrink it, 64 is the
/// identity) — except an unsigned width in `48..64`, which could exceed the range and takes the
/// fit-guarded store defensively.
fn emit_mask_width(
    cg: &mut Codegen,
    dst: Reg,
    src: Reg,
    signed: bool,
    bits: u8,
    pc: usize,
    op_blocks: &[Block],
) {
    let x = if cg.kind_claim(pc, src) == Kind::Int {
        cg.read_raw_int(src)
    } else {
        let v = cg.read_reg(src);
        let is_int = cg.is_small_int(v);
        let ok = cg.b.create_block();
        guard(cg, is_int, ok, pc);
        let x = cg.unbox_int(v);
        cg.def_raw(src, x);
        x
    };
    let m = emit_mask_to_width(cg, x, signed, bits);
    if !signed && (48..64).contains(&bits) {
        box_int_and_store(cg, dst, m, pc, op_blocks);
    } else {
        store_int_unchecked(cg, dst, m, pc, op_blocks);
    }
}

/// The float body of a `Binary`, entered with both operands proven f64 floats (J2)./// The float body of a `Binary`, entered with both operands proven f64 floats (J2). Computes in f64
/// and stores the boxed result. Matches the interpreter's `arithmetic`/`compare`: ordered
/// comparisons (false on NaN, except `!=` which is true on NaN), and a NaN arithmetic result
/// canonicalized to the standard quiet NaN — exactly `Value::float`. `%` calls the `fmod` helper
/// (S2 — no Cranelift instruction exists, and `a - trunc(a/b)*b` is not bit-exact to fmod).
fn emit_float_binary(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    va: ClValue,
    vb: ClValue,
    pc: usize,
    op_blocks: &[Block],
) {
    let x = cg.bits_to_f64(va);
    let y = cg.bits_to_f64(vb);
    emit_float_binary_vals(cg, op, dst, x, y, pc, op_blocks);
}

/// [`emit_float_binary`] on operands already converted to f64 — shared with the mixed int/float
/// lane (S2), which widens its int side before entering.
fn emit_float_binary_vals(
    cg: &mut Codegen,
    op: BinaryOp,
    dst: Reg,
    x: ClValue,
    y: ClValue,
    pc: usize,
    op_blocks: &[Block],
) {
    // Float `%` (S2): one call to the `fmod` runtime helper — Rust `%` on f64, the interpreter's
    // exact semantics; the NaN-canonicalizing store below matches the other float ops.
    if op == BinaryOp::Rem {
        let call = cg.b.ins().call(cg.fmod_ref, &[x, y]);
        let r = cg.b.inst_results(call)[0];
        box_float_and_store(cg, dst, r, pc, op_blocks);
        return;
    }
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
            let boxed =
                cg.b.ins()
                    .select(cmp, cg.pool.true_bits, cg.pool.false_bits);
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
    let boxed = cg.b.ins().select(is_nan, cg.pool.nan_canon, raw);
    cg.store_reg(dst, boxed);
    cg.b.ins().jump(op_blocks[pc + 1], &[]);
}

/// Emit the **cancellation poll** at loop header `pc` (isolate-cancel, JIT half): load the run's
/// flag and, if it is set, bail to the interpreter at `pc`.
///
/// **The poll decides nothing.** It does not unwind, does not touch the abort path, and does not
/// consult the flag's meaning — it deopts, and the interpreter (which is about to re-execute this
/// very pc) makes the call at its own back-edge safepoint. That keeps every rule about *when* a
/// cancellation may be honored in one place: notably `Vm::run_destructor` lifts the flag for the
/// duration of a destructor, and because native code only ever bails, the JIT cannot truncate a
/// destructor no matter what the flag says while one is running. It also means the poll needs no
/// deopt contract of its own: bailing at a pc is the mechanism every guard in this file already
/// uses, and it shares that pc's bail block.
///
/// **Placement.** A loop header — the target of a backward branch ([`backward_target`]) — is
/// entered exactly once per iteration, so this is the native analogue of the interpreter's
/// `osr_backedge!` poll. Polling the header rather than the branch also means the *entry* into a
/// loop polls, and one poll covers every back-edge of a multi-`continue` loop.
///
/// **The load is an `atomic_load`, not a plain load, and that is load-bearing** rather than
/// pedantic: the flag is written by another thread, and `atomic_load` carries
/// `other_side_effects`, so Cranelift's mid-end (running at `opt_level=speed`) can neither hoist
/// it out of the loop nor fold two iterations' polls together. A plain load would be free to do
/// both, and the failure mode is exactly the bug this whole slice exists to remove — a loop that
/// checks once and then never again. On x86-64 an `atomic_load` of `I8` lowers to a single
/// `movzbl`; the interpreter's own poll is a `Relaxed` load of the same byte.
fn emit_cancel_poll(cg: &mut Codegen, pc: usize) {
    let addr = cg.b.ins().iconst(types::I64, cg.cancel_addr as i64);
    let flag =
        cg.b.ins()
            .atomic_load(types::I8, MemFlagsData::trusted(), addr);
    // Keep going iff the flag is still clear (`AtomicBool` is one byte, `false == 0`).
    let keep_going = cg.b.ins().icmp_imm(IntCC::Equal, flag, 0);
    let cont = cg.b.create_block();
    guard(cg, keep_going, cont, pc);
}

/// Emit a fast-path guard: `brif cond -> cont else bail(pc)` and leave the builder positioned in
/// `cont` so the caller keeps emitting the fast path. `cont` is a caller-created block; `cond` is
/// the keep-going condition (true = stay in native code). The bail block — which spills the
/// SSA-resident live registers, then hands control back to the interpreter at `pc` — is **shared
/// per pc** ([`Codegen::bail_blocks`]): created and filled by the pc's first guard, reused by the
/// rest. No sealing here — blocks are sealed once at the end (`seal_all_blocks`), which also
/// resolves the SSA variables' block parameters (P-JSSA), including the per-predecessor values
/// the shared bail's spills observe.
fn guard(cg: &mut Codegen, cond: ClValue, cont: Block, pc: usize) {
    match cg.bail_blocks[pc] {
        Some(bail) => {
            cg.b.ins().brif(cond, cont, &[], bail, &[]);
        }
        None => {
            let bail = cg.b.create_block();
            cg.bail_blocks[pc] = Some(bail);
            cg.b.ins().brif(cond, cont, &[], bail, &[]);
            cg.b.switch_to_block(bail);
            cg.sync_frame(pc);
            let here = cg.pc_const(pc);
            cg.ret_bail(here);
        }
    }
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

    /// P-AOT L3.0: the *same* codegen the runtime JIT uses now targets a `cranelift_object`
    /// backend. Compile a real program's every prototype into a relocatable object via
    /// `Jit::<ObjectModule>` — reusing the identical `emit_*` routines, no finalize — and prove it
    /// finishes to a well-formed host object file. This is the generalization proof; it does *not*
    /// run the emitted code (a native body bakes an absolute `frame_template` pointer meaningless in
    /// a relocatable object — running AOT bodies correctly is L3.1).
    #[test]
    fn object_backend_compiles_the_same_bodies_into_an_object_file() {
        use noeta_compiler::compile;
        noeta_stdlib::registry::default_seeded();
        use noeta_lexer::lex;
        use noeta_parser::parse;
        use noeta_span::{Source, SourceId};

        let src = "fn add(a: int, b: int): int { return a + b }\necho add(2, 3)\n";
        let source = Source::new(SourceId::FIRST, "aot_smoke.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).expect("program is in the VM subset");
        assert!(
            !module.protos.is_empty(),
            "program has prototypes to compile"
        );

        // A well-formed-but-dummy layout + template: only their shapes matter for *emission* (offsets
        // become constants; the template address is baked as an immediate, never dereferenced here).
        let template = [0u8; 64];
        let layout = FrameLayout {
            frame_size: 64,
            frame_align: 8,
            proto_offset: 0,
            base_offset: 8,
            pc_offset: 16,
            ret_dst_offset: 24,
            ret_transform_offset: 32,
            upvalues_offset: 40,
            vec_ptr_word: 0,
            vec_len_word: 1,
            vec_cap_word: 2,
        };
        let mut aot = Jit::<ObjectModule>::new_object("aot_smoke", layout, template.as_ptr())
            .expect("object backend builds");
        for proto in 0..module.protos.len() {
            aot.compile_object(&module, proto)
                .unwrap_or_else(|e| panic!("proto {proto} defines into the object: {e}"));
        }
        let obj = aot.finish().expect("object file emits");
        assert!(!obj.is_empty(), "the object file has bytes");
        // The host object carries a valid header (ELF on Linux).
        #[cfg(target_os = "linux")]
        assert_eq!(&obj[..4], b"\x7fELF", "emits a host ELF object");
    }

    /// P-AOT L3.1b: the eager whole-module driver compiles *every* eligible prototype into the object
    /// and the manifest's symbols are actually defined in it. Reads the emitted ELF's symbol table and
    /// asserts each native prototype's main (and fast) symbol is a real definition — the contract the
    /// L3.2 runtime binding depends on.
    #[test]
    fn aot_compile_module_emits_every_native_proto_as_a_defined_symbol() {
        use noeta_compiler::compile;
        noeta_stdlib::registry::default_seeded();
        use noeta_lexer::lex;
        use noeta_parser::parse;
        use noeta_span::{Source, SourceId};
        use object::{Object, ObjectSection, ObjectSymbol};

        // Several eligible functions + a call chain, so there are multiple native prototypes (and at
        // least one fast-convention body).
        let src = "fn sq(n: int): int { return n * n }\n\
                   fn add(a: int, b: int): int { return a + b }\n\
                   fn work(n: int): int { return add(sq(n), n) }\n\
                   echo work(6)\n";
        let source = Source::new(SourceId::FIRST, "aot_module.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).expect("program is in the VM subset");

        let template = [0u8; 64];
        let layout = FrameLayout {
            frame_size: 64,
            frame_align: 8,
            proto_offset: 0,
            base_offset: 8,
            pc_offset: 16,
            ret_dst_offset: 24,
            ret_transform_offset: 32,
            upvalues_offset: 40,
            vec_ptr_word: 0,
            vec_len_word: 1,
            vec_cap_word: 2,
        };
        let mut aot = Jit::<ObjectModule>::new_object("aot_module", layout, template.as_ptr())
            .expect("object backend builds");
        let manifest = aot.compile_module(&module).expect("whole module compiles");
        let obj = aot.finish().expect("object file emits");

        assert_eq!(
            manifest.protos.len(),
            module.protos.len(),
            "one manifest entry per prototype"
        );
        assert!(
            manifest.native_count() > 0,
            "the program has eligible prototypes compiled to native code"
        );

        // Every symbol the manifest claims must be a real definition in the object.
        let file = object::File::parse(&*obj).expect("valid object file");
        let defined: std::collections::HashSet<String> = file
            .symbols()
            .filter(|s| s.is_definition())
            .filter_map(|s| s.name().ok().map(str::to_string))
            .collect();
        for (p, entry) in manifest.protos.iter().enumerate() {
            if let Some(sym) = &entry.symbol {
                assert!(
                    defined.contains(sym),
                    "proto {p} main symbol {sym} is defined in the object; have {defined:?}"
                );
            }
            if let Some(fsym) = &entry.fast_symbol {
                assert!(
                    defined.contains(fsym),
                    "proto {p} fast symbol {fsym} is defined in the object"
                );
            }
        }

        // P-AOT L3.2a: the dispatch table is a defined data symbol, and every native entry (main +
        // fast) is a relocation from it to the proto's body — the wiring the runtime binding reads.
        assert!(
            defined.contains(AOT_DISPATCH_SYMBOL),
            "the AOT dispatch table symbol is defined"
        );
        let expected_relocs = manifest
            .protos
            .iter()
            .map(|e| e.symbol.is_some() as usize + e.fast_symbol.is_some() as usize)
            .sum::<usize>();
        let body_relocs = file
            .sections()
            .flat_map(|s| s.relocations().collect::<Vec<_>>())
            .filter_map(|(_, r)| match r.target() {
                object::RelocationTarget::Symbol(i) => file.symbol_by_index(i).ok(),
                _ => None,
            })
            .filter_map(|s| s.name().ok().map(str::to_string))
            .filter(|n| n.starts_with("noeta_jit_proto") || n.starts_with("noeta_jit_fast"))
            .count();
        assert_eq!(
            body_relocs, expected_relocs,
            "the dispatch table relocates to exactly every native main+fast body"
        );
    }

    /// **Every AOT-emitted body starts on a [`noeta_jit_abi::MIN_BODY_ALIGNMENT`] boundary**, so bit
    /// 0 of an entry pointer belongs to [`noeta_jit_abi::FAST_ENTRY_TAG`] and not to the address.
    ///
    /// This is the exact property whose absence crashed every `--native` build of
    /// `modules/derived_package_path`: `cranelift-object` places a body at
    /// `max(buffer.alignment, isa.symbol_alignment())`, both of which are **1** on x86-64, so it
    /// packed bodies back to back and one landed odd. `jit_prepare_call`'s `ff | 1` was then a no-op
    /// and the caller's `& !1` called `ff - 1` — a `ret` that handed the address itself back as the
    /// callee's outcome, which `jit_after_call` stored as a bytecode pc.
    ///
    /// The assertion is over **both halves of the placement**, because either alone is satisfiable
    /// by accident: each body symbol sits at an aligned offset *within* its section, and the section
    /// carries at least that alignment so the linker cannot shift the whole block onto an odd
    /// address. Before the `log2_min_function_alignment` fix the section alignment is 1 and this
    /// fails deterministically, rather than depending on how the emitted bodies happened to size.
    #[test]
    fn aot_bodies_are_aligned_so_the_fast_convention_tag_is_a_free_bit() {
        use noeta_compiler::compile;
        noeta_stdlib::registry::default_seeded();
        use noeta_lexer::lex;
        use noeta_parser::parse;
        use noeta_span::{Source, SourceId};
        use object::{Object, ObjectSection, ObjectSymbol};

        // Odd-sized bodies of several different shapes, so the "next body lands odd" case is live:
        // a leaf returning a heap constant is exactly the shape whose fast body landed at an odd
        // address in the crashing artifact.
        let src = "fn a(): string { return \"a\" }\n\
                   fn b(): string { return \"bb\" }\n\
                   fn c(n: int): int { return n * n + 1 }\n\
                   fn d(n: int): int { return c(n) + 7 }\n\
                   echo \"${a()} ${b()} ${d(3)}\"\n";
        let source = Source::new(SourceId::FIRST, "aot_align.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).expect("program is in the VM subset");

        let template = [0u8; 64];
        let layout = FrameLayout {
            frame_size: 64,
            frame_align: 8,
            proto_offset: 0,
            base_offset: 8,
            pc_offset: 16,
            ret_dst_offset: 24,
            ret_transform_offset: 32,
            upvalues_offset: 40,
            vec_ptr_word: 0,
            vec_len_word: 1,
            vec_cap_word: 2,
        };
        let mut aot = Jit::<ObjectModule>::new_object("aot_align", layout, template.as_ptr())
            .expect("object backend builds");
        let manifest = aot.compile_module(&module).expect("whole module compiles");
        let obj = aot.finish().expect("object file emits");

        let bodies: Vec<String> = manifest
            .protos
            .iter()
            .flat_map(|e| e.symbol.iter().chain(e.fast_symbol.iter()))
            .cloned()
            .collect();
        assert!(
            bodies.len() > 2,
            "the program emits several bodies (main + fast) to place: {bodies:?}"
        );

        let align = noeta_jit_abi::MIN_BODY_ALIGNMENT as u64;
        let file = object::File::parse(&*obj).expect("valid object file");
        let mut checked = 0usize;
        for sym in file.symbols().filter(|s| s.is_definition()) {
            let Ok(name) = sym.name() else { continue };
            if !bodies.iter().any(|b| b == name) {
                continue;
            }
            assert_eq!(
                sym.address() % align,
                0,
                "body {name} is placed at offset {:#x}, which is not {align}-byte aligned — bit 0 \
                 of its entry pointer belongs to FAST_ENTRY_TAG, not to the address",
                sym.address(),
            );
            let section = sym
                .section_index()
                .and_then(|i| file.section_by_index(i).ok())
                .expect("a defined body lives in a section");
            assert!(
                section.align() >= align,
                "the section holding {name} is {}-byte aligned, so the linker may place the whole \
                 block on an odd address and undo the per-symbol alignment",
                section.align(),
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            bodies.len(),
            "every manifest body was found in the object's symbol table and checked"
        );
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
                    type_args: noeta_bytecode::TypeArgs::NONE,
                    span: sp,
                    supplied: None,
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
