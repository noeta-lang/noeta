//! The `task` concurrency module (higher-order-abi H0): the async combinators, registered
//! through the higher-order **ctx** dispatch table because they need the executor (and, for the
//! combinators, closure call-backs) — capabilities the plain value-in/value-out registry seam
//! deliberately does not carry.
//!
//! `sleep` is the seam's proving client (H0): the simplest family member, one shared dispatch
//! body replacing a `Builtin` arm written twice. `all`/`race`/`map_bounded` are still backend
//! builtins (`registry::VIRTUAL_MODULES`) and migrate here in H2.

use noeta_native::registry::{ExtFn, RetTy, SigType};
use noeta_native::{type_error, CtxError, CtxOut, NativeCtx, NativeValue, Scalar, Slot};

pub const TASK_CTX_FNS: &[ExtFn] = &[
    // `sleep(ms) -> Future<void>` (Track A.2): a leaf timer future, ready once the executor's
    // clock reaches `now + ms`.
    ExtFn {
        name: "sleep",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Future(&SigType::Unit)),
    },
];

pub fn task_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "sleep" => {
            noeta_native::ctx_arity(func, args, 1)?;
            let NativeValue::Scalar(Scalar::Int(ms)) = ctx.view(args[0])? else {
                return Err(type_error(func, "int").into());
            };
            if ms < 0 {
                return Err(type_error(func, "non-negative duration").into());
            }
            Ok(CtxOut::Slot(ctx.timer(ms as u64)))
        }
        _ => Err(noeta_native::no_function_error("task", func).into()),
    }
}
