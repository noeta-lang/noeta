//! `std.log` — the logs SDK surface (native OTEL, Phase L).
//!
//! A facade over the [`Logging`](noeta_native::Logging) Host capability. Emits OTel `LogRecord`s:
//! structured, exported log lines — **not** a `print`. The defining feature is **automatic
//! trace-correlation**: a record carries the [`TraceContext`] of the span active when it was
//! emitted (read from the same task-local active-span stack `std.tracing` maintains), so a log
//! inside a `with_span` reads back with that span's trace/span id and a top-level log has none —
//! zero threading by the user.
//!
//! Because the record reaches the active-span stack, these are **ctx functions** (like the tracing
//! surface), not plain-dispatch module functions. They gate on [`Logging::tel_logs_enabled`]: with
//! no logs endpoint configured (real host) the call is the enable check and a return — a hot
//! `log.info(...)` loop pays nothing. Under the deterministic sandbox the recorder is always on, so
//! the parity oracle observes every record.
//!
//! Surface: `log(severity, message)` (severity parsed case-insensitively; unknown → `info`) plus the
//! `debug`/`info`/`warn`/`error` conveniences. `trace`/`fatal` are reachable through the generic
//! `log`. Structured attributes (`*_with(message, attrs)`) arrive in L2.

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    AttrValue, CtxError, CtxOut, CtxResult, LogRecord, NativeCtx, NativeValue, Scalar, Severity,
    Slot, TraceContext, ctx_arity, no_function_error, type_error,
};

/// An OTel attribute value — the scalar union the surface accepts (a non-scalar is a compile-time
/// type error, not a runtime one). Same union `std.tracing`'s attributes use.
const ATTR_VALUE: SigType =
    SigType::Union(&[SigType::String, SigType::Int, SigType::Float, SigType::Bool]);

/// The structured-attributes parameter — `Map<string, string|int|float|bool>` (the `*_with` forms).
const ATTR_MAP: SigType = SigType::Map(&SigType::String, &ATTR_VALUE);

/// `std.log`'s functions — all ctx functions (they read the active-span stack for correlation).
/// The generic `log(severity, message)` takes a severity string; the conveniences take just a
/// message.
pub const LOG_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        name: "log",
        params: &[SigType::String, SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "debug",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "info",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "warn",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "error",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    // Structured-attribute forms: a `Map<string, string|int|float|bool>` of extra attributes.
    ExtFn {
        name: "log_with",
        params: &[SigType::String, SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "debug_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "info_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "warn_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "error_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// `std.log` ctx dispatch. Generic over the ctx (`C: NativeCtx + ?Sized`) so a compiled-in backend
/// inlines the small ctx ops, exactly as `tracing`/`cell`/`reactive` are.
pub fn log_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    // Resolve (severity, message-slot, optional attributes-slot) across the generic form, the
    // per-level conveniences, and their `*_with` structured-attribute variants.
    let (severity, msg_slot, attrs_slot) = match func {
        "log" => {
            ctx_arity(func, args, 2)?;
            // Gate before materializing any argument — the whole point of the enable check.
            if !ctx.host().tel_logs_enabled() {
                return Ok(unit());
            }
            (severity_from_str(&slot_str(ctx, args[0])?), args[1], None)
        }
        "log_with" => {
            ctx_arity(func, args, 3)?;
            if !ctx.host().tel_logs_enabled() {
                return Ok(unit());
            }
            (
                severity_from_str(&slot_str(ctx, args[0])?),
                args[1],
                Some(args[2]),
            )
        }
        "debug" | "info" | "warn" | "error" => {
            ctx_arity(func, args, 1)?;
            if !ctx.host().tel_logs_enabled() {
                return Ok(unit());
            }
            (level_of(func), args[0], None)
        }
        "debug_with" | "info_with" | "warn_with" | "error_with" => {
            ctx_arity(func, args, 2)?;
            if !ctx.host().tel_logs_enabled() {
                return Ok(unit());
            }
            (level_of(func), args[0], Some(args[1]))
        }
        _ => return Err(no_function_error("log", func).into()),
    };
    let body = slot_str(ctx, msg_slot)?;
    let attributes = match attrs_slot {
        Some(slot) => slot_attrs(ctx, slot)?,
        None => Vec::new(),
    };
    emit(ctx, severity, body, attributes);
    Ok(unit())
}

/// Build and emit one [`LogRecord`], stamping the active span's context (the correlation link) and
/// the host clock. The caller has already confirmed the logs signal is enabled.
fn emit<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    severity: Severity,
    body: String,
    attributes: Vec<(compact_str::CompactString, AttrValue)>,
) {
    let trace_context = current_parent(ctx);
    let unix_ms = ctx.host().clock_unix_ms();
    ctx.host().log_emit(LogRecord {
        unix_ms,
        severity,
        body: body.into(),
        attributes,
        trace_context,
    });
}

/// The W3C context of the current active span — the record's correlation link, or `None` at top
/// level. Mirrors `tracing`'s reader over the task-local active-span stack.
fn current_parent<C: NativeCtx + ?Sized>(ctx: &mut C) -> Option<TraceContext> {
    let top = ctx.task_context().top()?;
    Some(ctx.host().tel_span_context(top))
}

/// The severity of a per-level convenience (`info`/`info_with` → [`Severity::Info`], …).
fn level_of(func: &str) -> Severity {
    match func.strip_suffix("_with").unwrap_or(func) {
        "debug" => Severity::Debug,
        "warn" => Severity::Warn,
        "error" => Severity::Error,
        _ => Severity::Info,
    }
}

/// Project a `Map<string, string|int|float|bool>` argument into log attributes. The checker
/// constrains the value type to that scalar union, so a non-scalar can only arrive through a `dyn`
/// launder (a runtime type error then).
fn slot_attrs<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    slot: Slot,
) -> CtxResult<Vec<(compact_str::CompactString, AttrValue)>> {
    match ctx.view(slot)? {
        NativeValue::Map(entries) => entries
            .into_iter()
            .map(|(k, v)| Ok((k.into(), attr_from_native(&v)?)))
            .collect(),
        _ => Err(type_error("log", "map").into()),
    }
}

/// Project one map value into an [`AttrValue`] (str/int/float/bool).
fn attr_from_native(v: &NativeValue) -> CtxResult<AttrValue> {
    match v {
        NativeValue::Str(s) => Ok(AttrValue::Str(s.as_str().into())),
        NativeValue::Scalar(Scalar::Int(i)) => Ok(AttrValue::Int(*i)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(AttrValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(AttrValue::Float(*f as f64)),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(AttrValue::Bool(*b)),
        _ => Err(type_error("log", "string, int, float, or bool").into()),
    }
}

/// Parse a severity name (case-insensitive) for the generic `log(severity, message)`. Accepts the
/// six OTel levels plus `warning`; anything unknown falls back to `info` (a log is never dropped for
/// a bad level).
fn severity_from_str(s: &str) -> Severity {
    match s.trim().to_ascii_lowercase().as_str() {
        "trace" => Severity::Trace,
        "debug" => Severity::Debug,
        "warn" | "warning" => Severity::Warn,
        "error" => Severity::Error,
        "fatal" => Severity::Fatal,
        _ => Severity::Info,
    }
}

fn slot_str<C: NativeCtx + ?Sized>(ctx: &mut C, slot: Slot) -> CtxResult<String> {
    match ctx.view(slot)? {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error("log", "string").into()),
    }
}

fn unit() -> CtxOut {
    CtxOut::Out(NativeOut::Unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_parsing_is_case_insensitive_with_info_fallback() {
        assert_eq!(severity_from_str("ERROR"), Severity::Error);
        assert_eq!(severity_from_str(" Warn "), Severity::Warn);
        assert_eq!(severity_from_str("warning"), Severity::Warn);
        assert_eq!(severity_from_str("fatal"), Severity::Fatal);
        assert_eq!(severity_from_str("nonsense"), Severity::Info);
    }

    #[test]
    fn convenience_levels_map_as_expected() {
        assert_eq!(level_of("debug"), Severity::Debug);
        assert_eq!(level_of("info"), Severity::Info);
        assert_eq!(level_of("warn"), Severity::Warn);
        assert_eq!(level_of("error"), Severity::Error);
    }
}
