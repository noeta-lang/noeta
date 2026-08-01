//! The cranelift-free **ABI contract** between the JIT/AOT codegen ([`noeta-jit`](../noeta_jit/index.html))
//! and the runtime that runs native bodies (the `noeta_jit_*` helpers, the AOT dispatch binding, and
//! the frame-entry gate — all in `noeta-vm`).
//!
//! # Why it is its own crate
//!
//! An **ahead-of-time** binary runs pre-compiled native bodies through a static dispatch table and
//! never JIT-compiles anything (`run_module_aot` binds the table with `self.jit == None`). It needs
//! the runtime *support* — the [`FrameLayout`] the bodies were baked against, the [`CompiledFn`] entry
//! type, the `noeta_jit_*` helper **names** the bodies call, the [`CallSiteCache`] slot shape, the
//! [`OUTCOME_CALLED`]-family return sentinels, and the [`AOT_DISPATCH_SYMBOL`] contract — but **not**
//! the Cranelift compiler (~20 MB) that produced them. Keeping this surface cranelift-free lets
//! `noeta-vm`'s `aot` feature depend on it *without* pulling `noeta-jit`/Cranelift.
//!
//! `noeta-jit` depends on and `pub use`s this crate, so every existing `noeta_jit::FrameLayout` /
//! `noeta_jit::CompiledFn` / `noeta_jit::OUTCOME_*` path keeps resolving unchanged.

use core::ffi::c_void;

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
/// for this symbol when it constructs the `Jit`.
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

/// **Fast-convention tag** (P-JSSA S4.1): the bit `jit_prepare_call` sets in the entry pointer it
/// returns to say "this is a *fast*-convention body — its window was reserved uninitialized and the
/// arguments travel as machine arguments". The native caller tests it and strips it (`& !1`) before
/// the indirect call; a clear bit means the classic [`CompiledFn`] convention.
///
/// Tagging a pointer only works while the bit is **free**, which is the whole content of
/// [`MIN_BODY_ALIGNMENT`]: on an odd entry address `ptr | FAST_ENTRY_TAG == ptr`, so the tag says
/// nothing and the caller's `& !FAST_ENTRY_TAG` lands one byte *before* the real body. Everything
/// that produces an entry pointer — the runtime JIT's `finalize_ptr`, the AOT dispatch table's
/// linker-resolved slots — owes this alignment, and `jit_install` refuses an entry that breaks it
/// rather than passing a pointer the tag cannot describe.
pub const FAST_ENTRY_TAG: usize = 1;

/// The alignment every compiled body's entry point must have, in bytes — the precondition
/// [`FAST_ENTRY_TAG`] rests on. Cranelift guarantees nothing here on its own (x86-64's
/// `function_alignment().minimum` and `symbol_alignment()` are both 1, so the object backend packs
/// bodies back to back and one can land odd), so `noeta_jit` sets the `log2_min_function_alignment`
/// flag to this on both the JIT and the object ISA. Only bit 0 is load-bearing; 16 is the target's
/// ordinary function alignment and is what the codegen actually asks for.
pub const MIN_BODY_ALIGNMENT: usize = 16;
/// The leaf-heap-op helper (J4): runs a single non-dispatching heap/collection op (the interpreter's
/// exact arm, refcounts included) and returns [`OUTCOME_CONTINUE`] (done — the caller advances) or a
/// resume pc (it can't handle this instance — a dispatch or an error — so the interpreter runs it).
pub const LEAF_OP_HELPER: &str = "noeta_jit_run_leaf_op";

/// `noeta_jit_fmod(a: f64, b: f64) -> f64` — float `%` (S2). fmod is a libcall, not a Cranelift
/// instruction, and `a - trunc(a/b)*b` is not bit-exact to it (the divide rounds), so native code
/// calls this helper, whose body is Rust's `%` on f64 — the interpreter's exact semantics.
pub const FMOD_HELPER: &str = "noeta_jit_fmod";

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
/// register but never as a cached-callee closure key, so it is a safe "empty" sentinel.
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

/// The export symbol name of prototype `p`'s main native body. A **single source of truth** shared
/// by codegen (the `Linkage::Export` name), the AOT manifest, and — at L3.2 — the runtime that binds
/// these symbols back into its per-proto entry tables. (Proto index is the stable dispatch key the
/// interpreter, JIT, AOT, and any future hot-reload all agree on.)
pub fn proto_symbol(p: usize) -> String {
    format!("noeta_jit_proto{p}")
}

/// The export symbol name of prototype `p`'s fast-convention body (P-JSSA S4.1), if it has one.
pub fn fast_symbol(p: usize) -> String {
    format!("noeta_jit_fast{p}")
}

/// The export symbol name of prototype `p`'s bail stub (an ineligible prototype's placeholder body).
pub fn stub_symbol(p: usize) -> String {
    format!("noeta_jit_stub{p}")
}

/// The exported data symbol carrying the AOT dispatch table (P-AOT L3.2, approach A). **ABI** (the
/// runtime binding in L3.2b must match exactly): pointer-width little-endian words —
/// `[count][main_0, fast_0, main_1, fast_1, …, main_{count-1}, fast_{count-1}]`. Word 0 is the
/// prototype count; then two pointer slots per prototype in index order — its main native body and
/// its fast-convention body — each a linker-resolved code address, or **null** where the prototype
/// is interpreted (no native body) or has no fast body. The runtime reads this one static at startup
/// and `jit_install`s each non-null entry into its mutable per-proto dispatch tables.
pub const AOT_DISPATCH_SYMBOL: &str = "noeta_aot_dispatch";
