//! `std.io` — the program's standard output/error streams (CLI-completion slice 1).
//!
//! Four functions that write a value's display form to one of the two **observable output** buffers
//! the differential oracle compares:
//!
//! - `out(x)` / `outln(x)` → the stdout buffer (the same buffer the `echo` keyword writes to);
//! - `err(x)` / `errln(x)` → the stderr buffer.
//!
//! The `*ln` variants append a trailing newline; the bare ones write raw. They reach the buffers
//! through [`NativeCtx::write_stdout`] / [`NativeCtx::write_stderr`] — the seam that lets an ordinary
//! native touch the compared output without a lowerer intrinsic or bytecode change.
//!
//! The argument is rendered through [`NativeCtx::render`] — the backend's **own** `Value::display`
//! path, the exact one `echo` / `Op::Stringify` use, *including* a re-entry into a user `to_string`.
//! There is deliberately **no** display logic in this module: `io.outln(x)` is a byte-for-byte twin
//! of `echo x` for every value, and both backends stay identical by construction (the render is
//! their canonical routine, not a re-derivation). `echo` (the stdout-line keyword) is untouched;
//! `io.outln(x)` is its programmatic twin.

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{CtxError, CtxOut, NativeCtx, Slot, ctx_arity, no_function_error};

/// `std.io`'s functions — all ctx functions (they reach the backend's stdout/stderr buffers and its
/// canonical render through the [`NativeCtx`] seam). Each takes any value (`Dyn`) and returns unit.
pub const IO_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "out",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "outln",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "err",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "errln",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// `std.io` ctx dispatch. Generic over the concrete ctx (`C: NativeCtx + ?Sized`) so a compiled-in
/// backend inlines the small write ops, exactly as `tracing`/`log`/`cell` are.
pub fn io_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    ctx_arity(func, args, 1)?;
    // Render through the backend's own display path (echo-identical, `to_string`-aware).
    let text = ctx.render(args[0])?;
    match func {
        "out" => ctx.write_stdout(&text),
        "outln" => {
            ctx.write_stdout(&text);
            ctx.write_stdout("\n");
        }
        "err" => ctx.write_stderr(&text),
        "errln" => {
            ctx.write_stderr(&text);
            ctx.write_stderr("\n");
        }
        _ => return Err(no_function_error("io", func).into()),
    }
    Ok(CtxOut::Out(NativeOut::Unit))
}
