//! `http.serve` — the bundled server's accept→dispatch→reply loop (higher-order-abi H3), the
//! third and largest client of the **ctx** dispatch seam. One shared body replaces the
//! per-backend `Builtin::Serve` arms; the loop is line-for-line the drive loop both backends
//! duplicated (http-server S3/S3b), so the sandbox interleaving stays deterministic and identical
//! by construction.
//!
//! **Concurrent (S3b):** each accepted connection's `handler(request)` is a task in a
//! server-owned in-flight set the loop reaps; a slow (async) handler yields at its awaits while
//! the next connection is accepted and other handlers advance — the cooperative Tier-1 model (the
//! accept future is polled alongside the handler futures each round, never drive-to-completion).
//! Under the sandbox the accept leaf drives the finite request script and reports the listener
//! closed, so the loop terminates in-oracle; on the real host it serves until the socket closes.
//! A handler abort becomes a 500 — the canonical "drop [`CtxError::Abort`] to recover" pattern
//! the ctx error design was shaped around.

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    ArgKind, ArgSpec, AttrValue, CtxError, CtxOut, CtxResult, EntryArg, EntryCall, ErrorKind,
    ExtCommand, NativeCtx, NativeValue, NetRequest, NetResponse, Scalar, Slot, SpanId, SpanKind,
    SpanStatus, StdError, TraceContext, ctx_arity, no_function_error, panic_error,
};

use crate::net::{REQUEST_TYPE_NAME, Request, request_header, request_path};

const REQUEST_SIG: SigType = SigType::Named(REQUEST_TYPE_NAME);

pub const HTTP_CTX_FNS: &[ExtFn] = &[
    // `serve(port, handler) -> void` — bind an inbound listener and run the accept loop, calling
    // `handler(request)` per connection. The handler's declared return is `dyn`: a sync handler
    // yields the `Response`, an async one a `Future<Response>` — both reaped identically.
    ExtFn {
        name: "serve",
        params: &[SigType::Int, SigType::Fn(&[REQUEST_SIG], &SigType::Dyn)],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The `noeta serve` CLI subcommand (higher-order-abi H6) — the ergonomic entry point over an
/// explicit `server.serve(...)` call: run the file's top-level setup, then synthesize and run
/// `server.serve(<port>, fetch)`. The program supplies `fetch` and `use std.http.server` (binding
/// the local `server`); a missing one surfaces as an ordinary check error. Single worker,
/// cooperatively concurrent; runs until interrupted (Ctrl-C).
pub const SERVE_COMMAND: ExtCommand = ExtCommand {
    name: "serve",
    about: "Serve a program's HTTP handler (a top-level `fn fetch(req: Request): Response`)",
    args: &[
        ArgSpec {
            name: "file",
            help: "Path to a `.noe` file exporting a `fetch` handler",
            kind: ArgKind::Path,
        },
        ArgSpec {
            name: "port",
            help: "The TCP port to bind, default 8080 (the listener binds all interfaces, `0.0.0.0`)",
            kind: ArgKind::Int { default: 8080 },
        },
    ],
    run: |ctx, args| {
        let port = args.int("port");
        ctx.run_file(
            args.path("file"),
            Some(&EntryCall {
                module: "server",
                func: "serve",
                args: vec![EntryArg::Int(port), EntryArg::Ident("fetch")],
            }),
            Some(&format!(
                "noeta serve: listening on http://0.0.0.0:{port} (Ctrl-C to stop)"
            )),
        )
    },
};

/// One accepted connection's handler in flight: where to reply, the handler future the loop reaps,
/// the SERVER span it runs under (T4; `None` when tracing is off), and its **task-local context**
/// (T5b) — seeded with that span and swapped in around every call/poll of the handler, so its spans
/// nest under its own request and interleaved handlers stay isolated.
struct InFlight {
    conn: u64,
    fut: Slot,
    span: Option<SpanId>,
    context: Vec<u64>,
}

/// The reply for a handler that errors or returns a non-`Response`.
fn server_error() -> NetResponse {
    NetResponse {
        status: 500,
        headers: Vec::new(),
        body: b"Internal Server Error".to_vec(),
    }
}

/// Reply on `conn` — an async leaf, driven to completion (a write is quick).
fn reply(ctx: &mut dyn NativeCtx, conn: u64, response: NetResponse) -> CtxResult<()> {
    let io = ctx.host().net_reply(conn, response);
    let future = ctx.spawn_io(io);
    let unit = ctx.drive(future)?;
    ctx.free(unit);
    Ok(())
}

pub fn http_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "serve" => {
            ctx_arity(func, args, 2)?;
            let NativeValue::Scalar(Scalar::Int(port)) = ctx.view(args[0])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!(
                        "`http.serve` expects an int port, found {}",
                        ctx.type_name(args[0])?
                    ),
                }
                .into());
            };
            let handler = args[1];
            let addr = format!("0.0.0.0:{port}");
            let listener = ctx.host().net_listen(&addr)?;
            // Auto-instrumentation gate: only wrap requests in a SERVER span when telemetry is
            // actually configured, so an unconfigured `noeta serve` does zero span work per request.
            let tracing = ctx.host().tel_enabled();
            // Each in-flight handler carries the SERVER span it runs under (`None` when tracing is
            // off), ended when the handler replies so the span's duration is the request's — plus
            // its own **task-local context** seeded with that span (T5b): handler futures are polled
            // *manually* here (they are not scheduler tasks), so the loop swaps each handler's
            // context in around its call/polls, mirroring the scheduler's own per-task discipline.
            // A handler's `with_span`s then nest under its request's SERVER span, its `spawn`ed
            // tasks inherit it, and interleaved handlers cannot see each other's scope.
            let mut in_flight: Vec<InFlight> = Vec::new();
            let mut accept_future: Option<Slot> = None;
            let mut closing = false;
            loop {
                // Keep one accept in flight while the listener is open.
                if !closing && accept_future.is_none() {
                    let io = ctx.host().net_accept(listener);
                    accept_future = Some(ctx.spawn_io(io));
                }
                let mut progressed = false;
                // Poll the pending accept; on a connection, spawn its handler task. (An abort
                // here is the listener itself failing — propagate, unlike a handler abort.)
                if let Some(af) = accept_future.take() {
                    match ctx.poll(af)? {
                        Some(accepted) => {
                            progressed = true;
                            match ctx.option_payload(accepted)? {
                                Some(request) => {
                                    ctx.free(accepted);
                                    let conn = request_conn(ctx, request)?;
                                    // Open the SERVER span (parented on the inbound `traceparent`)
                                    // before the handler runs, so its start marks request arrival.
                                    let span = if tracing {
                                        Some(start_server_span(ctx, request)?)
                                    } else {
                                        None
                                    };
                                    // The handler's task-local context, seeded with its SERVER
                                    // span (empty when tracing is off — the swaps stay no-ops).
                                    let mut context: Vec<u64> =
                                        span.map(|s| vec![s]).unwrap_or_default();
                                    // Spawn the handler under its own context. A sync handler
                                    // returns the `Response` immediately (its whole body runs
                                    // inside this call — under the context); an async one a
                                    // `Future` reaped below. A call-time abort → 500 now.
                                    let prior = ctx.context_swap(std::mem::take(&mut context));
                                    let called = ctx.call(handler, &[request]);
                                    context = ctx.context_swap(prior);
                                    ctx.free(request);
                                    match called {
                                        Ok(fut) => in_flight.push(InFlight {
                                            conn,
                                            fut,
                                            span,
                                            context,
                                        }),
                                        Err(CtxError::Abort) => {
                                            end_server_span(ctx, span, 500);
                                            reply(ctx, conn, server_error())?;
                                        }
                                        Err(e) => {
                                            end_server_span(ctx, span, 500);
                                            return Err(e);
                                        }
                                    }
                                }
                                // `none` → the listener closed; stop accepting and drain.
                                None => {
                                    ctx.free(accepted);
                                    closing = true;
                                }
                            }
                        }
                        None => accept_future = Some(af),
                    }
                }
                // Reap: poll each in-flight handler; reply on completion, 500 on abort.
                let mut k = 0;
                while k < in_flight.len() {
                    let (conn, fut, span) = {
                        let e = &in_flight[k];
                        (e.conn, e.fut, e.span)
                    };
                    // Poll this handler under its own context (T5b): the swap pair mirrors the
                    // scheduler's per-task discipline, so a resumed handler sees exactly the scope
                    // it suspended with — never a sibling's.
                    let handler_ctx = std::mem::take(&mut in_flight[k].context);
                    let prior = ctx.context_swap(handler_ctx);
                    let polled = ctx.poll(fut);
                    in_flight[k].context = ctx.context_swap(prior);
                    let done = match polled {
                        Ok(Some(value)) => {
                            let mut response = None;
                            // A non-extern or non-`Response` result falls to the 500.
                            let _ = ctx.with_extern(value, &mut |e| {
                                response = e.as_any().downcast_ref::<NetResponse>().cloned();
                            });
                            ctx.free(value);
                            let response = response.unwrap_or_else(server_error);
                            // End the span with the reply's status before writing it, so the span's
                            // duration is the handler's and its status reflects the outcome.
                            end_server_span(ctx, span, response.status);
                            reply(ctx, conn, response)?;
                            true
                        }
                        Ok(None) => false,
                        Err(CtxError::Abort) => {
                            ctx.free(fut);
                            end_server_span(ctx, span, 500);
                            reply(ctx, conn, server_error())?;
                            true
                        }
                        Err(e) => return Err(e),
                    };
                    if done {
                        in_flight.remove(k);
                        progressed = true;
                    } else {
                        k += 1;
                    }
                }
                // Done when the listener closed and every handler has replied.
                if closing && in_flight.is_empty() && accept_future.is_none() {
                    return Ok(CtxOut::Out(NativeOut::Unit));
                }
                // Let a handler's own `concurrent` tasks advance, then the clock if stalled.
                // (No external-wake term — matching the migrated arms: a serve loop stalled with
                // no accept, no handler progress, and no timer is a genuine deadlock.)
                progressed |= ctx.advance_tasks()?;
                if !progressed && ctx.advance_clock().is_none() {
                    return Err(panic_error(
                        "async deadlock: `http.serve` stalled with no pending work",
                    )
                    .into());
                }
            }
        }
        _ => Err(no_function_error("http", func).into()),
    }
}

/// Open the auto-instrumentation **SERVER** span for an accepted request: named `"{method} {route}"`,
/// parented on the inbound W3C `traceparent` (so the server span continues the client's trace; a
/// missing/malformed header → a fresh root, the forgiving-reader rule), with the OTel HTTP
/// semantic-convention request attributes. The handler's spans do not auto-nest under it yet — that
/// needs per-task async context (T5); this span is the correctly-parented, timed root per request.
fn start_server_span(ctx: &mut dyn NativeCtx, request: Slot) -> CtxResult<SpanId> {
    // Pull the owned span inputs out under the extern borrow, then start the span (which needs the
    // host) once the borrow has ended.
    let mut inputs: Option<ServerSpanInputs> = None;
    ctx.with_extern(request, &mut |e| {
        if let Some(r) = e.as_any().downcast_ref::<Request>() {
            inputs = Some(server_span_inputs(&r.inner));
        }
    })?;
    let ServerSpanInputs {
        name,
        method,
        route,
        parent,
    } = inputs.expect("accept yields a Request extern value");
    let span = ctx.host().tel_span_start(&name, SpanKind::Server, parent);
    ctx.host()
        .tel_span_set_attr(span, "http.request.method", AttrValue::Str(method.into()));
    ctx.host()
        .tel_span_set_attr(span, "url.path", AttrValue::Str(route.into()));
    Ok(span)
}

/// The pure inputs a request contributes to its SERVER span, split off the ctx seam so the OTel
/// naming/attribute/parent-extraction is unit-tested directly.
struct ServerSpanInputs {
    name: String,
    method: String,
    route: String,
    parent: Option<TraceContext>,
}

/// Derive the SERVER span inputs from an inbound request: OTel name `"{method} {route}"` (the route
/// is the path with any query stripped), the method/route for the HTTP semantic-convention
/// attributes, and the parent parsed from an inbound W3C `traceparent` (absent/malformed → no parent,
/// i.e. a new root — the forgiving-reader rule).
fn server_span_inputs(req: &NetRequest) -> ServerSpanInputs {
    let method = req.method.clone();
    let route = request_path(&req.url).to_string();
    let name = format!("{method} {route}");
    let parent = request_header(req, "traceparent").and_then(TraceContext::parse);
    ServerSpanInputs {
        name,
        method,
        route,
        parent,
    }
}

/// End a SERVER span (a no-op when tracing is off, i.e. `span` is `None`): record the response's
/// status code and, per the OTel HTTP convention, mark the span an error only for a `5xx` (a `4xx`
/// is the client's fault, not a server error), then end it.
fn end_server_span(ctx: &mut dyn NativeCtx, span: Option<SpanId>, status: u16) {
    let Some(span) = span else { return };
    ctx.host().tel_span_set_attr(
        span,
        "http.response.status_code",
        AttrValue::Int(status as i64),
    );
    if let Some(status) = server_span_error_status(status) {
        ctx.host().tel_span_set_status(span, status);
    }
    ctx.host().tel_span_end(span);
}

/// The OTel span status for a SERVER span given its HTTP response code: `Error` only for `5xx`
/// (server fault); `4xx` and below leave the status unset (a client error is not a server error).
fn server_span_error_status(status: u16) -> Option<SpanStatus> {
    (status >= 500).then(|| SpanStatus::Error(format!("HTTP {status}").into()))
}

/// The `conn` id riding inside the accepted [`Request`] extern value — where the loop replies.
fn request_conn(ctx: &mut dyn NativeCtx, request: Slot) -> CtxResult<u64> {
    let mut conn = None;
    ctx.with_extern(request, &mut |e| {
        conn = e.as_any().downcast_ref::<Request>().map(|r| r.conn);
    })?;
    Ok(conn.expect("accept yields a Request extern value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, url: &str, headers: &[(&str, &str)]) -> NetRequest {
        NetRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    /// The span name is `"{method} {route}"` and the route is the path with any query string
    /// stripped — the low-cardinality shape OTel wants (a raw query would explode span cardinality).
    #[test]
    fn server_span_name_is_method_and_route_without_query() {
        let s = server_span_inputs(&req("GET", "/users/42?active=true", &[]));
        assert_eq!(s.name, "GET /users/42");
        assert_eq!(s.method, "GET");
        assert_eq!(s.route, "/users/42");
        assert!(s.parent.is_none(), "no traceparent → a root span");
    }

    /// An inbound `traceparent` becomes the SERVER span's parent, so the span continues the client's
    /// trace; a malformed header is ignored (forgiving reader) and the span is a fresh root.
    #[test]
    fn server_span_extracts_inbound_traceparent() {
        let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let parent = server_span_inputs(&req("GET", "/", &[("traceparent", tp)])).parent;
        assert_eq!(parent, TraceContext::parse(tp));
        assert!(parent.is_some());

        let malformed = server_span_inputs(&req("GET", "/", &[("traceparent", "garbage")])).parent;
        assert!(malformed.is_none(), "a malformed header yields no parent");
    }

    /// Per the OTel HTTP convention a SERVER span is an error only for `5xx`; `2xx`/`4xx` leave the
    /// status unset (a client error is not the server's fault).
    #[test]
    fn server_span_error_status_only_on_5xx() {
        assert!(server_span_error_status(200).is_none());
        assert!(server_span_error_status(404).is_none());
        assert_eq!(
            server_span_error_status(503),
            Some(SpanStatus::Error("HTTP 503".into()))
        );
    }
}
