//! The higher-order dispatch seam (higher-order-abi H0): how a native function that takes
//! **closures**, drives the **executor**, or orchestrates many futures crosses the extension ABI.
//!
//! The plain [`crate::registry::ModuleDispatch`] is value-in/value-out — it cannot carry a closure
//! (a closure is backend-specific; [`crate::NativeValue`] has no variant for it and could not
//! call one anyway), interleave more than one async spawn, or advance the scheduler mid-call. So
//! every function needing those was a hardcoded per-backend `Builtin`, written twice with
//! hand-maintained refcount discipline. This seam replaces that: the extension manipulates
//! **opaque slots** — indices into a per-call table of backend values it never sees — and
//! re-enters the backend through one capability trait, [`NativeCtx`]. Each backend implements the
//! trait once; the **slot table owns the refcount discipline centrally** (the VM retains on
//! insert and releases on free/drop), so a dispatch cannot leak the way the hand-written builtins
//! repeatedly did. The dispatch function itself stays a single shared `fn` in the extension crate,
//! so the differential holds by construction — the registry's core promise, extended to
//! orchestration code. See `plans/higher-order-abi/README.md`.

use crate::executor::ExternIo;
use crate::host::Host;
use crate::registry::{NativeOut, NativeValue};
use crate::StdError;

/// An opaque handle to one backend value held in the per-call slot table. Slots are **owned by
/// the table**: every method returning a `Slot` mints a fresh one (methods never consume argument
/// slots), [`NativeCtx::free`] releases one early (long loops must — the table otherwise lives to
/// the end of the dispatch), and the backend releases whatever remains when the dispatch returns.
pub type Slot = u32;

/// Why a ctx operation failed.
#[derive(Debug)]
pub enum CtxError {
    /// A native-level misuse or IO failure — the backend reports it as a diagnostic, exactly like
    /// a plain registry dispatch error.
    Std(StdError),
    /// User code aborted (a runtime diagnostic / panic) during a re-entry ([`NativeCtx::call`],
    /// [`NativeCtx::poll`], …). The diagnostic is **already recorded on the backend**; this is
    /// only the propagation token (mirroring the backends' own unit abort markers). Propagate it
    /// to unwind — or drop it to recover, the way `http.serve` turns a handler abort into a 500.
    Abort,
}

impl From<StdError> for CtxError {
    fn from(e: StdError) -> CtxError {
        CtxError::Std(e)
    }
}

pub type CtxResult<T> = Result<T, CtxError>;

/// Exact-arity guard for a ctx dispatch's slot arguments — the [`crate::arity_error`] twin of the
/// plain dispatches' `want_arity`.
pub fn ctx_arity(func: &str, args: &[Slot], expected: usize) -> CtxResult<()> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(crate::arity_error(func, expected, args.len()).into())
    }
}

/// What a [`CtxDispatch`] returns: a slot whose value becomes the call's result verbatim, or a
/// neutral [`NativeOut`] the backend materializes (for plain data results).
#[derive(Debug)]
pub enum CtxOut {
    Slot(Slot),
    Out(NativeOut),
}

/// A module's higher-order dispatch: like [`crate::registry::ModuleDispatch`], but arguments
/// arrive as opaque slots and the body may re-enter the backend through the [`NativeCtx`]
/// capability. One per module, shared by both backends.
pub type CtxDispatch =
    fn(func: &str, ctx: &mut dyn NativeCtx, args: &[Slot]) -> Result<CtxOut, CtxError>;

/// The backend capability a higher-order dispatch runs against. Implemented once per backend as
/// a thin wrapper over the interpreter/VM plus the slot table.
///
/// Growth points (later phases): option/enum inspection (http.serve's accept outcome, H3),
/// per-run extension state + retained handles (Class 3, H4), a reactive-graph view (H5).
pub trait NativeCtx {
    /// The host capability seam — filesystem, clock, network, … (what a plain dispatch receives).
    fn host(&mut self) -> &mut dyn Host;

    /// Marshal a slot's value into the neutral argument view (the deep projection — these are
    /// orchestration paths, never hot loops; elements that stay opaque ride as slots instead).
    /// Errs on a freed/invalid slot (dispatch misuse).
    fn view(&mut self, slot: Slot) -> CtxResult<NativeValue>;

    /// Materialize a neutral result into a fresh slot (the inverse of [`NativeCtx::view`]).
    /// Shape-relative outs ([`NativeOut::Object`], whose shape comes from a signature's
    /// `SameAsArg`) and [`NativeOut::Spawn`] (use [`NativeCtx::spawn_io`]) are misuse here.
    fn intern(&mut self, out: NativeOut) -> CtxResult<Slot>;

    /// Release a slot early. Freeing an already-freed slot is a no-op; using it again is misuse
    /// (the table returns a unit-ish placeholder / errors, it never touches freed memory).
    fn free(&mut self, slot: Slot);

    /// Call a callable slot (a closure, function handle, bound method — anything the language can
    /// call) with argument slots, re-entering the interpreter. Argument slots are **not**
    /// consumed; the result arrives in a fresh slot. An abort in the callee returns
    /// [`CtxError::Abort`] (recorded backend-side), which the dispatch may propagate or recover.
    fn call(&mut self, callee: Slot, args: &[Slot]) -> CtxResult<Slot>;

    /// Length of a list slot ([`CtxError::Std`] with an `ArgType` error if not a list).
    fn list_len(&mut self, list: Slot) -> CtxResult<usize>;

    /// Element `index` of a list slot, as a fresh slot.
    fn list_get(&mut self, list: Slot, index: usize) -> CtxResult<Slot>;

    /// Build a list value from element slots (elements are not consumed), as a fresh slot.
    fn make_list(&mut self, items: &[Slot]) -> CtxResult<Slot>;

    /// Ticket an async descriptor on the executor; the resulting leaf future arrives as a slot
    /// (the ctx twin of returning [`NativeOut::Spawn`] from a plain dispatch — but many may be
    /// in flight at once).
    fn spawn_io(&mut self, io: Box<dyn ExternIo>) -> Slot;

    /// A leaf timer future that becomes ready once the executor clock reaches `now + ms`
    /// (`task.sleep`'s value).
    fn timer(&mut self, ms: u64) -> Slot;

    /// Poll a future slot once: `Some(result-slot)` when ready, `None` while pending (the
    /// deadline/waker registration happens inside, as in the backends' own `poll_once`). The
    /// future slot itself stays valid (free it after it resolves).
    fn poll(&mut self, future: Slot) -> CtxResult<Option<Slot>>;

    /// Give the program's own `concurrent`-scope tasks one scheduling round; `true` if any task
    /// progressed (what `http.serve` interleaves with its accept loop).
    fn advance_tasks(&mut self) -> CtxResult<bool>;

    /// Advance the executor clock to the next pending timer deadline (`None` if there is none) —
    /// the stall escape valve of every drive loop.
    fn advance_clock(&mut self) -> Option<u64>;
}
