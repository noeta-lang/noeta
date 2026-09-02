//! `std.io` — the program's standard output/error streams.
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

use noeta_ext_abi::args::{want_arity, want_str};
use noeta_ext_abi::registry::{ExtFn, NativeOut, NativeValue, RetTy, Scalar, SigType};
use noeta_ext_abi::{
    CtxError, CtxOut, Host, NativeCtx, Slot, StdError, Stream, ctx_arity, no_function_error,
};

/// `std.io`'s functions — all ctx functions (they reach the backend's stdout/stderr buffers and its
/// canonical render through the [`NativeCtx`] seam). Each takes any value (`Dyn`) and returns unit.
pub const IO_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["value"],
        name: "out",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["value"],
        name: "outln",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["value"],
        name: "err",
        params: &[SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["value"],
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

/// `std.io`'s **host-backed** functions — the stdin and terminal-ness
/// surface of the [`Console`] capability. Unlike the ctx functions above (which reach the backends'
/// output buffers), these are plain host effects — a scripted fixture in the sandbox, real I/O on
/// `RealHost` — so they marshal through the ordinary [`Host`] dispatch, exactly like `env`/`os`.
/// Their names are disjoint from [`IO_CTX_FNS`], so the module's two dispatch tables never collide.
pub const IO_FNS: &[ExtFn] = &[
    // The next line of stdin (without its newline), or `none` at EOF — pair with `??`/`while`.
    ExtFn {
        param_names: &[],
        name: "stdin_line",
        params: &[],
        ret: RetTy::Concrete(SigType::Option(&SigType::String)),
    },
    // All remaining stdin, read to EOF as one string.
    ExtFn {
        param_names: &[],
        name: "stdin_all",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    },
    // Whether standard *output* is a terminal — the "should I colorize?" check.
    ExtFn {
        param_names: &[],
        name: "is_tty",
        params: &[],
        ret: RetTy::Concrete(SigType::Bool),
    },
    // Whether standard *input* is a terminal (vs a pipe/file).
    ExtFn {
        param_names: &[],
        name: "stdin_is_tty",
        params: &[],
        ret: RetTy::Concrete(SigType::Bool),
    },
    // Write `msg` to the terminal now (bypassing the batch buffer) and read one line — the single
    // interactive path. `none` at EOF.
    ExtFn {
        param_names: &["message"],
        name: "prompt",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Option(&SigType::String)),
    },
];

/// `std.io`'s host-backed dispatch — mirrors `env_dispatch`: it threads the
/// [`Host`] in and routes each name to a [`Console`] method. The ctx functions (`out`/`outln`/…) go
/// through [`io_ctx_dispatch`] instead; the two name sets are disjoint, so the module's plain-then-ctx
/// resolution reaches whichever table declares the call.
pub fn io_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "stdin_line" => {
            want_arity(func, args, 0)?;
            // EOF is `none`, a present line is `some(line)` — the same `Option` shape `env.get` uses.
            Ok(match host.stdin_read_line() {
                Some(line) => NativeOut::Some(Box::new(NativeOut::Str(line))),
                None => NativeOut::None,
            })
        }
        "stdin_all" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.stdin_read_all()))
        }
        "is_tty" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(host.is_tty(Stream::Stdout))))
        }
        "stdin_is_tty" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(host.is_tty(Stream::Stdin))))
        }
        "prompt" => {
            want_arity(func, args, 1)?;
            let msg = want_str(func, args, 0)?;
            Ok(match host.prompt(msg) {
                Some(line) => NativeOut::Some(Box::new(NativeOut::Str(line))),
                None => NativeOut::None,
            })
        }
        _ => Err(no_function_error("io", func)),
    }
}
