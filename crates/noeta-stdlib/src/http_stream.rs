//! The language-facing halves of streaming HTTP (http-streaming arc): `FrameStream`'s incremental
//! read methods and `SseSink`'s write methods, plus the `server.sse` upgrade marker.
//!
//! Both handle types reach the `Network` capability by id, so their methods ride the executor and
//! therefore live in the **ctx** table — the same shape `Socket` uses (`crate::serve`), for the
//! same reason: `recv` hands back a `Future` the caller awaits, and a write is an async leaf driven
//! to completion.
//!
//! The framing/parse semantics are not here. They are in `noeta_ext_abi::stream`, dependency-free,
//! so the deterministic sandbox and the real reqwest-backed host cut bytes with the identical
//! decoder and the differential holds by construction.

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::stream::{
    FRAME_STREAM_TYPE_NAME, FRAME_TYPE_NAME, SSE_SINK_TYPE_NAME, frame_from_value, sse_comment_wire,
};
use noeta_ext_abi::{
    CtxError, CtxOut, ErrorKind, NativeCtx, NativeValue, Slot, StdError, ctx_arity,
};

/// The `Frame` value-struct signature, named once.
pub(crate) const FRAME_SIG: SigType = SigType::Named(FRAME_TYPE_NAME);
/// `?Frame` — what a `recv` resolves to (`none` = the body ended).
const OPT_FRAME: SigType = SigType::Option(&FRAME_SIG);

/// `FrameStream`'s ctx methods: `recv` returns a `Future<?Frame>` the reader awaits (`none` once
/// the body ends), `close` releases the connection early.
pub const FRAME_STREAM_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &[],
        name: "recv",
        params: &[],
        ret: RetTy::Concrete(SigType::Future(&OPT_FRAME)),
    },
    ExtFn {
        param_names: &[],
        name: "close",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// `SseSink`'s ctx methods. All three are driven to completion rather than returning a future: a
/// frame write is a short push onto an already-open connection, exactly like `Socket.send`, and
/// making the common case `sink.send(f)` instead of `sink.send(f).await` is worth far more than
/// overlapping one socket write.
pub const SSE_SINK_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        param_names: &["frame"],
        name: "send",
        params: &[FRAME_SIG],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &["text"],
        name: "comment",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        param_names: &[],
        name: "close",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The stream id riding inside a `FrameStream` receiver.
fn receiver_stream(ctx: &mut dyn NativeCtx, recv: Slot) -> Result<u64, CtxError> {
    let mut stream = None;
    ctx.with_extern(recv, &mut |e| {
        stream = e
            .as_any()
            .downcast_ref::<noeta_ext_abi::stream::FrameStream>()
            .map(|s| s.stream);
    })?;
    Ok(stream.expect("a FrameStream receiver wraps a FrameStream"))
}

/// The connection id riding inside an `SseSink` receiver.
fn receiver_conn(ctx: &mut dyn NativeCtx, recv: Slot) -> Result<u64, CtxError> {
    let mut conn = None;
    ctx.with_extern(recv, &mut |e| {
        conn = e
            .as_any()
            .downcast_ref::<noeta_ext_abi::stream::SseSink>()
            .map(|s| s.conn);
    })?;
    Ok(conn.expect("an SseSink receiver wraps an SseSink"))
}

/// `FrameStream`'s ctx-method dispatch.
pub fn frame_stream_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let stream = receiver_stream(ctx, recv)?;
    match method {
        "recv" => {
            ctx_arity(method, args, 0)?;
            let io = ctx.host().net_stream_recv(stream);
            Ok(CtxOut::Slot(ctx.spawn_io(io)))
        }
        "close" => {
            ctx_arity(method, args, 0)?;
            // A plain host call, not a driven leaf: releasing a reader has nothing to await, and
            // spawning a descriptor to do nothing asynchronously would only add a scheduler round.
            ctx.host().net_stream_close(stream)?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(noeta_ext_abi::no_method_error(FRAME_STREAM_TYPE_NAME, method).into()),
    }
}

/// `SseSink`'s ctx-method dispatch.
pub fn sse_sink_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let conn = receiver_conn(ctx, recv)?;
    match method {
        "send" => {
            ctx_arity(method, args, 1)?;
            let view = ctx.view(args[0])?;
            let Some(frame) = frame_from_value(&view) else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!(
                        "`SseSink.send` expects a {FRAME_TYPE_NAME}, found {}",
                        ctx.type_name(args[0])?
                    ),
                }
                .into());
            };
            write_wire(ctx, conn, frame.to_sse_wire())
        }
        "comment" => {
            ctx_arity(method, args, 1)?;
            let NativeValue::Str(text) = ctx.view(args[0])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!(
                        "`SseSink.comment` expects a string, found {}",
                        ctx.type_name(args[0])?
                    ),
                }
                .into());
            };
            write_wire(ctx, conn, sse_comment_wire(&text))
        }
        "close" => {
            ctx_arity(method, args, 0)?;
            sse_close(ctx, conn)?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(noeta_ext_abi::no_method_error(SSE_SINK_TYPE_NAME, method).into()),
    }
}

/// Push already-encoded event-stream bytes — an async leaf driven to completion, like the serve
/// loop's `reply`.
fn write_wire(ctx: &mut dyn NativeCtx, conn: u64, wire: String) -> Result<CtxOut, CtxError> {
    let io = ctx.host().net_sse_send(conn, wire);
    let future = ctx.spawn_io(io);
    let unit = ctx.drive(future)?;
    ctx.free(unit);
    Ok(CtxOut::Out(NativeOut::Unit))
}

/// End an event stream — the driven leaf both `SseSink.close()` and the serve loop's session reap
/// use, so a stream closed by the handler and one closed by the loop take the identical path.
pub fn sse_close(ctx: &mut dyn NativeCtx, conn: u64) -> Result<(), CtxError> {
    let io = ctx.host().net_sse_close(conn);
    let future = ctx.spawn_io(io);
    let unit = ctx.drive(future)?;
    ctx.free(unit);
    Ok(())
}

/// The marker a `fetch` handler returns via `server.sse(handler)` — the exact analogue of
/// [`crate::serve::WsUpgrade`], and language-typed `Response` for the same reason: a routing
/// handler keeps one `(Request) -> Response` signature whether it answers with a body, a socket,
/// or an event stream. The serve loop's reap recognizes the concrete Rust type and switches the
/// connection to streaming instead of replying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseUpgrade {
    /// The session handler, held in the retained arena until the loop takes it.
    pub handler: noeta_ext_abi::Retained,
}

impl noeta_ext_abi::ExternValue for SseUpgrade {
    fn type_identity(&self) -> &'static str {
        crate::net::RESPONSE_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn noeta_ext_abi::ExternValue) -> bool {
        other.as_any().downcast_ref::<SseUpgrade>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn noeta_ext_abi::ExternValue) -> Option<std::cmp::Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<sse upgrade>")
    }
    fn clone_box(&self) -> Box<dyn noeta_ext_abi::ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ext_abi::registry::Scalar;
    use noeta_ext_abi::stream::Frame;

    fn frame_fields(retry: NativeValue) -> Vec<(String, NativeValue)> {
        vec![
            ("event".to_string(), NativeValue::Str("token".to_string())),
            ("data".to_string(), NativeValue::Str("hi".to_string())),
            ("id".to_string(), NativeValue::Str("3".to_string())),
            ("retry".to_string(), retry),
        ]
    }

    /// The shallow projection: a registered native struct as a real `Instance`.
    fn instance(retry: NativeValue) -> NativeValue {
        NativeValue::Instance {
            class: FRAME_TYPE_NAME.to_string(),
            fields: frame_fields(retry),
        }
    }

    /// The deep (JSON-shaped) projection — what `ctx.view` actually hands `SseSink.send`: an object
    /// flattened to its fields in declared order, with an `Option` marshalled THROUGH its payload.
    fn deep(retry: NativeValue) -> NativeValue {
        NativeValue::Map(frame_fields(retry))
    }

    #[test]
    fn a_frame_reads_back_out_of_either_argument_projection() {
        let expected = Frame {
            event: "token".to_string(),
            data: "hi".to_string(),
            id: "3".to_string(),
            retry: None,
        };
        // The DEEP projection is what `ctx.view` — and therefore `SseSink.send` — actually
        // produces: an object flattened to its fields. Reading only the shallow `Instance` shape
        // is a silent no-op at every real call site, which is how this was first written.
        assert_eq!(
            frame_from_value(&deep(NativeValue::Unit)).expect("a Frame"),
            expected,
            "deep projection"
        );
        assert_eq!(
            frame_from_value(&instance(NativeValue::Unit)).expect("a Frame"),
            expected,
            "shallow projection"
        );
    }

    #[test]
    fn an_optional_retry_reads_in_either_marshalled_shape() {
        // Which shape an `?int` arrives in is the backend's marshalling detail; the seam accepts
        // both so a change there cannot silently drop a `retry:` hint.
        let flattened = frame_from_value(&instance(NativeValue::Scalar(Scalar::Int(1500))));
        assert_eq!(flattened.expect("a Frame").retry, Some(1500));

        let wrapped = frame_from_value(&instance(NativeValue::Variant {
            enum_name: "Option".to_string(),
            variant: "Some".to_string(),
            variant_index: 1,
            fields: vec![NativeValue::Scalar(Scalar::Int(1500))],
        }));
        assert_eq!(wrapped.expect("a Frame").retry, Some(1500));

        let absent = frame_from_value(&instance(NativeValue::Variant {
            enum_name: "Option".to_string(),
            variant: "None".to_string(),
            variant_index: 0,
            fields: vec![],
        }));
        assert_eq!(absent.expect("a Frame").retry, None);
    }

    #[test]
    fn a_non_instance_is_not_a_frame() {
        assert_eq!(frame_from_value(&NativeValue::Str("x".to_string())), None);
    }
}
