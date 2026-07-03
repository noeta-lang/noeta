//! The `lang` method JIT (milestone P-JIT) — a Cranelift backend that native-compiles hot
//! prototypes so the fast path runs as machine code instead of dispatched register bytecode.
//!
//! # Where this sits
//!
//! The interpreter ([`lang_vm`](../lang_vm/index.html)) is **tier 0**: every prototype runs by
//! `match`-dispatching its [`lang_bytecode::Op`]s. This crate is **tier 1**: a per-prototype
//! [`CompiledFn`] the VM may call *instead of* entering the inner dispatch loop for that frame.
//! A compiled function operates directly on the VM's shared contiguous register stack
//! (P-VMT-FRAME) at `regs[base + i]`, and returns an [`Outcome`] telling the interpreter what to
//! do next.
//!
//! # This slice — J0 (foundation)
//!
//! J0 wires the plumbing end to end and **compiles no real op yet**: [`Jit::compile`] emits, for
//! every prototype, a *bail stub* — native code that calls one runtime helper (proving the helper
//! ABI links and the VM pointer round-trips) and returns [`Outcome::Bail`], so the interpreter
//! runs the frame exactly as before. What J0 proves is the whole seam: Cranelift builds and
//! finalizes machine code, the VM dispatches through a finalized function pointer, a generated
//! call reaches a Rust runtime helper with the live VM pointer, and control falls back cleanly to
//! tier 0. The `--jit-differential` oracle (in `lang-conformance`) then asserts the JIT path is
//! byte-identical to the interpreter across the whole corpus, and the leak oracle proves refcount
//! parity — the gates every later slice (J1 integer fast path onward) must keep green.
//!
//! # Gating
//!
//! The whole JIT lives behind the `jit` cargo feature on `lang-vm`/`lang-conformance`. The default
//! build, the deterministic sandbox, and the conformance differential never pull Cranelift and are
//! byte-identical without it — the same discipline that gates the real-thread isolates.

use core::ffi::c_void;

use cranelift_codegen::ir::{AbiParam, InstBuilder, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _, default_libcall_names};

use lang_bytecode::Module;
use lang_value::Value;

/// A compiled prototype's entry point — the tier-1 ABI.
///
/// - `vm` is an opaque `*mut Vm` (the interpreter reconstitutes `&mut Vm` from it to service
///   runtime-helper callbacks); this crate never dereferences it.
/// - `regs` points at the base of the VM's shared register stack (`Vec<Value>`), and `base` is the
///   frame's window offset, so the frame's registers are `regs[base + i]` — identical addressing to
///   the interpreter (P-VMT-FRAME).
/// - The `u8` return is an [`Outcome`] discriminant.
///
/// `extern "C"` so Cranelift's `system_v`/platform calling convention matches the pointer this is
/// transmuted from.
pub type CompiledFn = unsafe extern "C" fn(vm: *mut c_void, regs: *mut Value, base: usize) -> u8;

/// What a [`CompiledFn`] tells the interpreter to do when it returns. The `#[repr(u8)]`
/// discriminants are the ABI contract — the compiled code returns these as raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    /// The compiled code declined (an unsupported op, or a guard that failed): the interpreter
    /// must run this frame in tier 0 from its current `pc`. J0 always returns this.
    Bail = 0,
    /// The compiled code ran the whole frame and performed the return protocol itself; the
    /// interpreter should pop the frame. Unused in J0 (no op is compiled yet) — reserved for J1+.
    Returned = 1,
}

impl Outcome {
    /// Map the raw `u8` a [`CompiledFn`] returns back to an [`Outcome`]. Any unexpected byte is
    /// treated as [`Outcome::Bail`] — the always-safe verdict (fall back to the interpreter).
    pub fn from_raw(raw: u8) -> Outcome {
        match raw {
            1 => Outcome::Returned,
            _ => Outcome::Bail,
        }
    }
}

/// The name the bail stub calls to prove the runtime-helper ABI. The VM registers a pointer for
/// this symbol when it constructs the [`Jit`]; J1+ registers the real `retain`/`release`/`call`
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
    /// The imported `lang_jit_observe` helper, declared once and referenced by every stub.
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
        // JIT code is called in-process; no colocated libcalls, no position-independent code.
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
            ctx,
            fb_ctx: FunctionBuilderContext::new(),
        })
    }

    /// The finalized entry point for prototype `proto`, or `None` if it is not compiled (tier 0).
    pub fn get(&self, proto: usize) -> Option<CompiledFn> {
        self.compiled.get(proto).copied().flatten()
    }

    /// How many prototypes are compiled — the JIT-coverage number the oracle reports.
    pub fn compiled_count(&self) -> usize {
        self.compiled.iter().filter(|c| c.is_some()).count()
    }

    /// Compile prototype `proto` of `module` and cache its entry point, returning it.
    ///
    /// J0: this emits a **bail stub** regardless of the prototype's body — native code that calls
    /// the `lang_jit_observe` helper with the VM pointer (proving the helper ABI) and returns
    /// [`Outcome::Bail`], so the interpreter then runs the frame. Idempotent: a second call for an
    /// already-compiled prototype returns the cached entry point.
    ///
    /// Returns `Err` if Cranelift fails to build or finalize (which leaves the prototype at tier 0).
    pub fn compile(&mut self, module: &Module, proto: usize) -> Result<CompiledFn, String> {
        if proto >= self.compiled.len() {
            self.compiled.resize(module.protos.len().max(proto + 1), None);
        }
        if let Some(f) = self.compiled[proto] {
            return Ok(f);
        }
        let f = self.emit_bail_stub(proto)?;
        self.compiled[proto] = Some(f);
        Ok(f)
    }

    /// Emit the J0 bail stub for prototype `proto` and return its finalized entry point.
    ///
    /// The generated body is `{ lang_jit_observe(vm); return Outcome::Bail; }` — one runtime-helper
    /// call and a constant return. Emitting the call is deliberate: it exercises the full helper ABI
    /// (symbol resolution, the VM-pointer argument, the platform calling convention) that J1+ leans
    /// on for `retain`/`release`/`call`.
    fn emit_bail_stub(&mut self, proto: usize) -> Result<CompiledFn, String> {
        let ptr_ty = self.module.target_config().pointer_type();

        // The tier-1 ABI signature: (vm: ptr, regs: ptr, base: usize) -> i8.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(ptr_ty)); // vm
        sig.params.push(AbiParam::new(ptr_ty)); // regs
        sig.params.push(AbiParam::new(ptr_ty)); // base (usize == pointer width)
        sig.returns.push(AbiParam::new(types::I8)); // Outcome discriminant

        // The runtime-helper `lang_jit_observe(vm: ptr)` — imported, resolved to the VM's pointer.
        let mut helper_sig = self.module.make_signature();
        helper_sig.params.push(AbiParam::new(ptr_ty));
        let helper_id = self
            .module
            .declare_function(OBSERVE_HELPER, Linkage::Import, &helper_sig)
            .map_err(|e| e.to_string())?;

        let func_id = self
            .module
            .declare_function(&format!("lang_jit_proto{proto}"), Linkage::Export, &sig)
            .map_err(|e| e.to_string())?;

        self.module.clear_context(&mut self.ctx);
        self.ctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fb_ctx);
            let block = b.create_block();
            b.append_block_params_for_function_params(block);
            b.switch_to_block(block);
            b.seal_block(block);

            let vm = b.block_params(block)[0];
            let helper_ref = self.module.declare_func_in_func(helper_id, b.func);
            b.ins().call(helper_ref, &[vm]);

            let bail = b.ins().iconst(types::I8, Outcome::Bail as i64);
            b.ins().return_(&[bail]);
            b.finalize();
        }
        self.module
            .define_function(func_id, &mut self.ctx)
            .map_err(|e| e.to_string())?;
        self.module.clear_context(&mut self.ctx);
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;

        let code = self.module.get_finalized_function(func_id);
        // SAFETY: `code` is a finalized function whose Cranelift signature is exactly the
        // `extern "C" fn(ptr, ptr, usize) -> i8` this transmutes to, and it stays valid for as long
        // as `self.module` (which owns the code page) lives.
        let f: CompiledFn = unsafe { std::mem::transmute::<*const u8, CompiledFn>(code) };
        Ok(f)
    }
}
