//! The `std.template` module — a **native expression-tier handler** (expr-tiers arc), the dogfood
//! of the extension expression-tier surface. std declares the `@json` tier through `ExtTier`
//! (`crate::tiers`): body language `json`, value type `string`, handler `std.template.render`. A
//! `@json { … ${s} … }` block desugars — like any expression tier — to `render(statics, holes)`,
//! resolved to *this* native function (a `NativeFnRef` callee, no user import), which interleaves
//! the verbatim statics with the JSON-quoted string holes.
//!
//! So a native package (here, std) ships an embedded language whose blocks are typed, checked
//! values with a native handler — the closures arrive as opaque slots and are invoked through the
//! [`NativeCtx`] higher-order capability, exactly like `cell.update`'s callback.

use noeta_ext_abi::registry::{ExtFn, NativeOut, NativeValue, RetTy, SigType};
use noeta_ext_abi::{CtxError, CtxOut, NativeCtx, Slot, ctx_arity, no_function_error};

/// `render(statics: List<string>, holes: List<() -> string>): string` — the `@json` handler. Holes
/// are `() -> string` (each rendered value JSON-quoted), so a `${value}` is always injected as a
/// safely-escaped JSON string — the point of an embedded, checked template over raw concatenation.
pub const TEMPLATE_CTX_FNS: &[ExtFn] = &[ExtFn {
    name: "render",
    params: &[
        SigType::List(&SigType::String),
        SigType::List(&SigType::Fn(&[], &SigType::String)),
    ],
    ret: RetTy::Concrete(SigType::String),
}];

pub fn template_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "render" => {
            ctx_arity(func, args, 2)?;
            let statics = args[0];
            let holes = args[1];
            let count = ctx.list_len(statics)?;
            let hole_count = ctx.list_len(holes)?;
            let mut out = String::new();
            for i in 0..count {
                let static_slot = ctx.list_get(statics, i)?;
                if let NativeValue::Str(s) = ctx.view(static_slot)? {
                    out.push_str(&s);
                }
                if i < hole_count {
                    // Invoke the hole closure (`fn() -> string`) and JSON-quote its value.
                    let hole = ctx.list_get(holes, i)?;
                    let value = ctx.call(hole, &[])?;
                    let text = match ctx.view(value)? {
                        NativeValue::Str(s) => s,
                        _ => String::new(),
                    };
                    out.push('"');
                    for c in text.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                }
            }
            Ok(CtxOut::Out(NativeOut::Str(out)))
        }
        _ => Err(no_function_error("template", func).into()),
    }
}
