//! `std.tracing` — the tracing SDK surface (native OTEL, T1–T3).
//!
//! A facade over the [`Tracing`](noeta_ext_abi::Tracing) Host capability. The span tree and
//! export live host-side (the sandbox recorder / the real OTLP exporter); this module owns only the
//! **active-span stack** that gives spans implicit parenting — exactly the design T0 chose (the
//! host is a pure span factory/sink, context management is the SDK's job). Since T5a the stack
//! lives in the backend's **task-local context** (`NativeCtx::context_*`): each task carries its
//! own, the scheduler swaps it in around the task's polls, and a `spawn`ed task inherits a snapshot
//! of its spawner's — so scope follows execution, not the run.
//!
//! - `span(name) -> Span` starts a span parented on the current active span (a root at top level)
//!   and returns a [`Span`] handle the caller ends itself.
//! - `with_span(name, body) -> A` is the **scoped** form (the no-RAII answer, Class-2 `ctx.call`):
//!   it starts a span, makes it the active parent for the duration of `body`, runs `body`, then ends
//!   the span — **even if `body` aborts** (it records an error status and re-propagates). Returns
//!   `body`'s value. An **async** body (T5c) returns its future and the span follows it through the
//!   backend's completion hook (`NativeCtx::trace_future`): every poll runs under the span's
//!   context (spans created after a suspension still nest correctly) and the span ends when the
//!   future completes — or aborts — so the duration is the work's, not the construction's.
//! - `current_context() -> str` is the current active span's W3C `traceparent` (empty when none) —
//!   the propagation **inject** side (serialize onto a channel message / outbound header).
//! - `span_from(name, traceparent) -> Span` is the propagation **extract** side: it starts a span
//!   whose parent is the remote context parsed from `traceparent`, continuing the inbound trace (a
//!   malformed header parses to no-parent → a new root, the W3C forgiving-reader rule). This is what
//!   crosses an isolate boundary — the `traceparent` is a plain string, so it rides a channel
//!   message as-is; the receiving isolate calls `span_from` to continue the same trace.
//! - `set_attribute(key, value)` / `add_event(name)` / `record_error(message) -> bool` annotate the
//!   **active** span — the span the caller is already *inside*, which no handle names. Deliberately
//!   free functions rather than a `current_span() -> ?Span`: the active span is an *ambient* value
//!   (`std.log` already reads this very stack with no handle at all), and materializing it into a
//!   `Span` would hand out `.end()` on a span the caller never opened — a `with_span` body could end
//!   the span `with_span` is about to end, and a request handler could end the auto-instrumented
//!   SERVER span out from under `http.serve`. A free function cannot express that hazard. The names
//!   mirror the `Span` methods exactly, so one operation keeps one name and `tracing.add_event` is
//!   where someone who knows `span.add_event` looks for it. The `bool` reports whether a **live**
//!   active span received the annotation — the "no active span" answer, which has to be *visible*
//!   for an API used to record errors, and which (being a value) the ordinary differential oracle
//!   observes, unlike the write-only spans themselves.
//! - `Span` methods `set_attribute`/`add_event`/`record_error`/`end` marshal to `host.tel_span_*`;
//!   `Span.context() -> str` reads a *held* span's own `traceparent` (inject a specific span, not
//!   just the active one).
//!
//! `span`/`with_span`/`current_context`/`span_from` and the three active-span annotators reach the
//! active stack and/or start spans, so they route through the `NativeCtx` seam; the `Span` methods
//! only touch the host, so they stay plain dispatch.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::args::{want_arity, want_str};
use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    AttrValue, CtxError, CtxOut, CtxResult, ExternBox, ExternValue, Host, NativeCtx, NativeValue,
    Scalar, Slot, SpanId, SpanKind, SpanStatus, StdError, TraceContext, ctx_arity,
    no_function_error, no_method_error, type_error,
};

/// The surface type name for a span handle. `Span` is a namespaced extern type, not a reserved
/// name — a user may declare their own `Span`; a clash arises only if `use std.tracing.Span` is
/// also in scope (an import conflict, E0020). E0049 is reserved for the checker-native generics
/// `Iterator`/`Future`/`Sender`/`Receiver`.
pub const SPAN_TYPE_NAME: &str = "Span";

/// `Span`'s qualified runtime identity — what [`crate::ExternValue::type_identity`] returns.
pub const SPAN_TYPE_IDENTITY: &str = "std.tracing.Span";

/// `with_span`'s body return type variable — `with_span(name, Fn() -> A) -> A`.
const VAR_A: SigType = SigType::Var(0);

/// An OTel attribute value — the scalar union the surface accepts (a non-scalar is a compile-time
/// type error, not a runtime one).
const ATTR_VALUE: SigType =
    SigType::Union(&[SigType::String, SigType::Int, SigType::Float, SigType::Bool]);

/// An OTel attribute *map* — the `add_event_with(name, attrs)` parameter. Same shape `std.log`'s
/// `*_with` forms and `std.metrics`' `add_with`/`record_with` take, so one attribute vocabulary
/// spans all three signals.
const ATTR_MAP: SigType = SigType::Map(&SigType::String, &ATTR_VALUE);

/// `std.tracing`'s functions — all higher-order (they reach the active-span stack, and `with_span`
/// calls a closure), so they live in the ctx table.
pub const TRACING_CTX_FNS: &[ExtFn] = &[
    ExtFn {
        param_names: &["name"],
        name: "span",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        param_names: &["name", "f"],
        name: "with_span",
        params: &[SigType::String, SigType::Fn(&[], &VAR_A)],
        ret: RetTy::Concrete(VAR_A),
    },
    ExtFn {
        param_names: &[],
        name: "current_context",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    },
    ExtFn {
        param_names: &["name", "traceparent"],
        name: "span_from",
        params: &[SigType::String, SigType::String],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    // The **active-span annotators**: the same three mutations `Span` offers, applied to the span
    // the caller is already inside rather than to a handle it holds. Same names as the methods (one
    // operation, one name); `bool` = "a live active span received it" (see the module header for
    // why this is a free function and not `current_span() -> ?Span`).
    ExtFn {
        param_names: &["key", "value"],
        name: "set_attribute",
        params: &[SigType::String, ATTR_VALUE],
        ret: RetTy::Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["name"],
        name: "add_event",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Bool),
    },
    // An event carrying its own attributes. This is the form a *structured* fact wants: several
    // facts recorded on one span each keep their own attribute set, where span-level
    // `set_attribute` would have them overwrite each other by key.
    ExtFn {
        param_names: &["name", "attrs"],
        name: "add_event_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Bool),
    },
    ExtFn {
        param_names: &["message"],
        name: "record_error",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Bool),
    },
];

/// The `Span` instance methods (plain dispatch — they only reach the host). The mutators chain
/// (return the span), so `span.set_attribute("a", 1).add_event("hit")` reads left to right; `end`
/// finalizes.
pub const SPAN_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["key", "value"],
        name: "set_attribute",
        params: &[SigType::String, ATTR_VALUE],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        param_names: &["name"],
        name: "add_event",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        param_names: &["name", "attrs"],
        name: "add_event_with",
        params: &[SigType::String, ATTR_MAP],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        param_names: &[],
        name: "context",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    },
    ExtFn {
        param_names: &["message"],
        name: "record_error",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Named(SPAN_TYPE_NAME)),
    },
    ExtFn {
        param_names: &[],
        name: "end",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The `Span` extern value: the host's opaque span handle (plain `Copy` data). Mutating methods
/// reach the host by id; the backend's extern cell gives it reference semantics, so a span passed
/// around refers to one host-side span. Not key-capable (it identifies a mutable host resource).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub id: SpanId,
}

impl ExternValue for Span {
    fn type_identity(&self) -> &'static str {
        SPAN_TYPE_IDENTITY
    }

    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Span>() == Some(self)
    }

    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }

    fn hash_value(&self) -> u64 {
        0 // not key-capable; never consulted
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<span {}>", self.id)
    }

    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(*self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `std.tracing` ctx dispatch. Generic over the ctx (`C: NativeCtx + ?Sized`) so a compiled-in
/// backend inlines the small ctx ops, exactly as `cell`/`reactive` are.
pub fn tracing_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "span" => {
            ctx_arity(func, args, 1)?;
            let name = slot_str(ctx, args[0])?;
            let parent = current_parent(ctx);
            let id = ctx.host().tel_span_start(&name, SpanKind::Internal, parent);
            Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(Span { id }))))
        }
        "with_span" => {
            ctx_arity(func, args, 2)?;
            let name = slot_str(ctx, args[0])?;
            let parent = current_parent(ctx);
            let id = ctx.host().tel_span_start(&name, SpanKind::Internal, parent);
            // Make the span the active parent for the body, then always pop — even on abort — so a
            // failed body cannot leave a dangling active span for the next call.
            push_active(ctx, id);
            let result = ctx.call(args[1], &[]);
            pop_active(ctx, id);
            match result {
                Ok(slot) => {
                    // An **async** body: `ctx.call` only constructed its future (lazily) — the
                    // work hasn't run. Hand the future to the backend's completion hook (T5c):
                    // its polls run under this span's context and the span ends when it
                    // completes, so the duration is the body's, not the construction's. A
                    // non-traceable future flavor falls back to ending now.
                    if ctx.type_name(slot)? == "future" && ctx.future_tracing().trace(slot, id)? {
                        return Ok(CtxOut::Slot(slot));
                    }
                    ctx.host().tel_span_end(id);
                    Ok(CtxOut::Slot(slot))
                }
                Err(err) => {
                    ctx.host()
                        .tel_span_set_status(id, SpanStatus::Error("span body aborted".into()));
                    ctx.host().tel_span_end(id);
                    Err(err)
                }
            }
        }
        "current_context" => {
            ctx_arity(func, args, 0)?;
            let traceparent = current_parent(ctx)
                .map(|c| c.to_traceparent())
                .unwrap_or_default();
            Ok(CtxOut::Out(NativeOut::Str(traceparent)))
        }
        "span_from" => {
            ctx_arity(func, args, 2)?;
            let name = slot_str(ctx, args[0])?;
            let traceparent = slot_str(ctx, args[1])?;
            // The extract side of propagation: a malformed inbound header parses to `None` and the
            // span becomes a new root (the W3C forgiving-reader rule), never an error. The remote
            // parent overrides implicit parenting — this span continues the *inbound* trace.
            let parent = TraceContext::parse(&traceparent);
            let id = ctx.host().tel_span_start(&name, SpanKind::Internal, parent);
            Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(Span { id }))))
        }
        // ----- the active-span annotators -----
        //
        // Each reads its arguments FIRST and resolves the active span second, so a malformed
        // argument is the same error whether or not a span happens to be active — the alternative
        // makes a program's typing depend on its trace context.
        "set_attribute" => {
            ctx_arity(func, args, 2)?;
            let key = slot_str(ctx, args[0])?;
            let value = slot_attr(ctx, args[1])?;
            Ok(annotated(match active_live_span(ctx) {
                Some(id) => {
                    ctx.host().tel_span_set_attr(id, &key, value);
                    true
                }
                None => false,
            }))
        }
        "add_event" | "add_event_with" => {
            let attrs = if func == "add_event_with" {
                ctx_arity(func, args, 2)?;
                slot_attr_map(ctx, args[1])?
            } else {
                ctx_arity(func, args, 1)?;
                Vec::new()
            };
            let name = slot_str(ctx, args[0])?;
            Ok(annotated(match active_live_span(ctx) {
                Some(id) => {
                    ctx.host().tel_span_add_event(id, &name, attrs);
                    true
                }
                None => false,
            }))
        }
        "record_error" => {
            ctx_arity(func, args, 1)?;
            let message = slot_str(ctx, args[0])?;
            Ok(annotated(match active_live_span(ctx) {
                Some(id) => {
                    ctx.host()
                        .tel_span_set_status(id, SpanStatus::Error(message.into()));
                    true
                }
                None => false,
            }))
        }
        _ => Err(no_function_error("tracing", func).into()),
    }
}

/// The annotators' answer: whether a live active span received the annotation.
fn annotated(hit: bool) -> CtxOut {
    CtxOut::Out(NativeOut::Scalar(Scalar::Bool(hit)))
}

/// `Span` method dispatch (plain — the mutators only reach the host).
pub fn span_method_dispatch(
    recv: &mut dyn ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let id = recv
        .as_any()
        .downcast_ref::<Span>()
        .expect("Span method dispatch on a non-Span extern")
        .id;
    match method {
        "set_attribute" => {
            want_arity(method, args, 2)?;
            let key = want_str(method, args, 0)?;
            let value = want_attr(method, args, 1)?;
            host.tel_span_set_attr(id, key, value);
            Ok(span_value(id))
        }
        "add_event" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            host.tel_span_add_event(id, name, Vec::new());
            Ok(span_value(id))
        }
        "add_event_with" => {
            want_arity(method, args, 2)?;
            let attrs = want_attr_map(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            host.tel_span_add_event(id, name, attrs);
            Ok(span_value(id))
        }
        "context" => {
            // This span's own W3C `traceparent` — the inject side for a *held* span (e.g. serialize
            // it onto a channel message or an outbound header), distinct from the active-span read
            // `tracing.current_context()`.
            want_arity(method, args, 0)?;
            let context = host.tel_span_context(id);
            Ok(NativeOut::Str(context.to_traceparent()))
        }
        "record_error" => {
            want_arity(method, args, 1)?;
            let message = want_str(method, args, 0)?;
            host.tel_span_set_status(id, SpanStatus::Error(message.into()));
            Ok(span_value(id))
        }
        "end" => {
            want_arity(method, args, 0)?;
            host.tel_span_end(id);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_method_error(SPAN_TYPE_NAME, method)),
    }
}

// ----- active-span stack (the backend's task-local context, T5a) -----
//
// The stack lives in the backend's per-strand context cell (`NativeCtx::context_*`), NOT in
// per-run `ExtState`: the scheduler swaps each task's own stack in around its polls and a spawned
// task inherits a snapshot of its spawner's, so tracing scope follows *execution* — two
// interleaved tasks' `with_span`s no longer see (or corrupt) each other's parents.

pub(crate) fn push_active<C: NativeCtx + ?Sized>(ctx: &mut C, id: SpanId) {
    ctx.task_context().push(id);
}

/// Pop `id` if it is the current top (defensive against a re-entrant push imbalance).
pub(crate) fn pop_active<C: NativeCtx + ?Sized>(ctx: &mut C, id: SpanId) {
    ctx.task_context().pop(id);
}

/// The W3C context of the current active span — a new span's implicit parent.
pub(crate) fn current_parent<C: NativeCtx + ?Sized>(ctx: &mut C) -> Option<TraceContext> {
    let top = ctx.task_context().top()?;
    Some(ctx.host().tel_span_context(top))
}

/// The **live** active span: the top of this task's stack, when that top is a real local span.
///
/// The stack's top is not always one. Automatic propagation (T5d) seeds a receiving strand with a
/// *remote-interned* pseudo-handle — a context that arrived on a channel message or across an
/// isolate boundary, whose originating span lives in another host. It parents correctly (that is
/// what it is for) but it is not live here, so every `tel_span_*` mutation no-ops on it. An
/// annotator that treated it as the active span would report success while dropping the annotation
/// on the floor, which is precisely the silent failure this surface exists to avoid — so a remote
/// seed reads as "nothing to annotate" and the caller gets `false`. (`current_parent` above
/// deliberately does *not* filter: a remote seed is a perfectly good implicit **parent**.)
pub(crate) fn active_live_span<C: NativeCtx + ?Sized>(ctx: &mut C) -> Option<SpanId> {
    let top = ctx.task_context().top()?;
    (!ctx.host().tel_is_remote(top)).then_some(top)
}

fn slot_str<C: NativeCtx + ?Sized>(ctx: &mut C, slot: Slot) -> CtxResult<String> {
    match ctx.view(slot)? {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error("tracing", "string").into()),
    }
}

/// Project an attribute-value **slot** into an [`AttrValue`] — the ctx twin of [`want_attr`], for
/// the active-span annotators (which read arguments as slots, not marshalled `NativeValue`s).
fn slot_attr<C: NativeCtx + ?Sized>(ctx: &mut C, slot: Slot) -> CtxResult<AttrValue> {
    let value = ctx.view(slot)?;
    Ok(attr_from_native("tracing", &value)?)
}

/// Project a `Map<string, string|int|float|bool>` **slot** into event attributes (the ctx path's
/// `add_event_with`). Ordering follows the map's own iteration order, which both backends share.
fn slot_attr_map<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    slot: Slot,
) -> CtxResult<Vec<(compact_str::CompactString, AttrValue)>> {
    match ctx.view(slot)? {
        NativeValue::Map(entries) => entries
            .into_iter()
            .map(|(k, v)| Ok((k.into(), attr_from_native("tracing", &v)?)))
            .collect(),
        _ => Err(type_error("tracing", "map").into()),
    }
}

/// Project a marshalled `Map` **argument** into event attributes (the `Span` handle path's
/// `add_event_with`). Requires `deep_marshal` on the `Span` extern type, so the map arrives whole
/// rather than as an opaque projection — the same reason the metrics handles set it.
fn want_attr_map(
    method: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<Vec<(compact_str::CompactString, AttrValue)>, StdError> {
    match &args[index] {
        NativeValue::Map(entries) => entries
            .iter()
            .map(|(k, v)| Ok((k.as_str().into(), attr_from_native(method, v)?)))
            .collect(),
        _ => Err(type_error(method, "map")),
    }
}

/// Project one attribute value (str/int/float/bool) out of a marshalled value — the single
/// projection every attribute position in this module funnels through (span attributes, event
/// attributes, both dispatch paths). The checker constrains those positions to [`ATTR_VALUE`], so a
/// non-scalar can only arrive through a `dyn` launder; `label` names the surface in that error.
fn attr_from_native(label: &str, v: &NativeValue) -> Result<AttrValue, StdError> {
    match v {
        NativeValue::Str(s) => Ok(AttrValue::Str(s.as_str().into())),
        NativeValue::Scalar(Scalar::Int(i)) => Ok(AttrValue::Int(*i)),
        NativeValue::Scalar(Scalar::Float(f)) => Ok(AttrValue::Float(*f)),
        NativeValue::Scalar(Scalar::F32(f)) => Ok(AttrValue::Float(*f as f64)),
        NativeValue::Scalar(Scalar::Bool(b)) => Ok(AttrValue::Bool(*b)),
        _ => Err(type_error(label, "string, int, float, or bool")),
    }
}

fn span_value(id: SpanId) -> NativeOut {
    NativeOut::Extern(ExternBox::new(Span { id }))
}

/// Project an attribute-value argument (str/int/float/bool) into an [`AttrValue`].
fn want_attr(method: &str, args: &[NativeValue], index: usize) -> Result<AttrValue, StdError> {
    attr_from_native(method, &args[index])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tracing; // the trait, so `tel_span_start` resolves on a concrete SandboxHost
    use crate::host::SandboxHost;

    fn str_arg(s: &str) -> NativeValue {
        NativeValue::Str(s.to_string())
    }

    /// The plain `Span` method path (set_attribute/add_event/record_error/end) against a span the
    /// host started directly — the ctx functions (`span`/`with_span`) are exercised through the
    /// conformance differential (both backends), as every ctx dispatch is.
    #[test]
    fn span_methods_marshal_to_the_recorder() {
        let mut host = SandboxHost::new();
        let id = host.tel_span_start("request", SpanKind::Internal, None);
        let mut span = Span { id };

        let chained = span_method_dispatch(
            &mut span,
            "set_attribute",
            &mut host,
            &[str_arg("http.method"), str_arg("GET")],
        )
        .unwrap();
        assert!(matches!(chained, NativeOut::Extern(_)));
        span_method_dispatch(&mut span, "add_event", &mut host, &[str_arg("cache.miss")]).unwrap();
        span_method_dispatch(&mut span, "record_error", &mut host, &[str_arg("boom")]).unwrap();
        assert!(host.recorded_spans().is_empty(), "not recorded until end()");
        assert_eq!(
            span_method_dispatch(&mut span, "end", &mut host, &[]).unwrap(),
            NativeOut::Unit
        );

        let spans = host.recorded_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "request");
        assert_eq!(
            spans[0].attributes,
            vec![("http.method".into(), AttrValue::Str("GET".into()))]
        );
        assert_eq!(spans[0].events.len(), 1);
        assert_eq!(spans[0].events[0].name, "cache.miss");
        assert_eq!(spans[0].status, SpanStatus::Error("boom".into()));
    }

    /// `Span.context()` returns the span's own W3C `traceparent`, and it round-trips through
    /// [`TraceContext::parse`] back to the host-reported context — the inject/extract pair used for
    /// cross-boundary propagation.
    #[test]
    fn span_context_round_trips_traceparent() {
        let mut host = SandboxHost::new();
        let id = host.tel_span_start("outbound", SpanKind::Internal, None);
        let mut span = Span { id };

        let out = span_method_dispatch(&mut span, "context", &mut host, &[]).unwrap();
        let NativeOut::Str(traceparent) = out else {
            panic!("context() returns a string");
        };
        let parsed = TraceContext::parse(&traceparent).expect("a live span's traceparent parses");
        assert_eq!(parsed, host.tel_span_context(id));

        span_method_dispatch(&mut span, "end", &mut host, &[]).unwrap();
    }

    #[test]
    fn unknown_method_is_an_error() {
        let mut host = SandboxHost::new();
        let mut span = Span { id: 1 };
        assert!(span_method_dispatch(&mut span, "nope", &mut host, &[]).is_err());
    }
}
