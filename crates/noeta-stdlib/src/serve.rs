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
    ArgKind, ArgSpec, CtxError, CtxOut, CtxResult, EntryArg, EntryCall, ErrorKind, ExtCommand,
    NativeCtx, NativeValue, NetResponse, Scalar, Slot, StdError, ctx_arity, no_function_error,
    panic_error,
};

use crate::net::{REQUEST_TYPE_NAME, Request};

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
/// explicit `http.serve(...)` call: run the file's top-level setup, then synthesize and run
/// `http.serve(<port>, fetch)`. The program supplies `fetch` and `use std.{http}`; a missing one
/// surfaces as an ordinary check error. Single worker, cooperatively concurrent; runs until
/// interrupted (Ctrl-C).
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
                module: "http",
                func: "serve",
                args: vec![EntryArg::Int(port), EntryArg::Ident("fetch")],
            }),
            Some(&format!(
                "noeta serve: listening on http://0.0.0.0:{port} (Ctrl-C to stop)"
            )),
        )
    },
};

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
            let mut in_flight: Vec<(u64, Slot)> = Vec::new();
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
                                    // Spawn the handler. A sync handler returns the `Response`
                                    // immediately; an async one a `Future` reaped below. A
                                    // call-time abort → 500 now.
                                    let called = ctx.call(handler, &[request]);
                                    ctx.free(request);
                                    match called {
                                        Ok(fut) => in_flight.push((conn, fut)),
                                        Err(CtxError::Abort) => reply(ctx, conn, server_error())?,
                                        Err(e) => return Err(e),
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
                    let (conn, fut) = in_flight[k];
                    let done = match ctx.poll(fut) {
                        Ok(Some(value)) => {
                            let mut response = None;
                            // A non-extern or non-`Response` result falls to the 500.
                            let _ = ctx.with_extern(value, &mut |e| {
                                response = e.as_any().downcast_ref::<NetResponse>().cloned();
                            });
                            ctx.free(value);
                            reply(ctx, conn, response.unwrap_or_else(server_error))?;
                            true
                        }
                        Ok(None) => false,
                        Err(CtxError::Abort) => {
                            ctx.free(fut);
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

/// The `conn` id riding inside the accepted [`Request`] extern value — where the loop replies.
fn request_conn(ctx: &mut dyn NativeCtx, request: Slot) -> CtxResult<u64> {
    let mut conn = None;
    ctx.with_extern(request, &mut |e| {
        conn = e.as_any().downcast_ref::<Request>().map(|r| r.conn);
    })?;
    Ok(conn.expect("accept yields a Request extern value"))
}
