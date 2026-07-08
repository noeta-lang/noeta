//! The `task` concurrency module (higher-order-abi H0/H2): the async combinators, registered
//! through the higher-order **ctx** dispatch table because they need the executor (and, for the
//! combinators, closure call-backs) — capabilities the plain value-in/value-out registry seam
//! deliberately does not carry.
//!
//! `sleep` was the seam's proving client (H0); `all`/`race`/`map_bounded` (H2) are the first
//! genuine orchestrators — they call closures back, poll many futures, drive the scheduler — and
//! their loops here are line-for-line the drive loops the deleted per-backend `Builtin` arms
//! duplicated. One shared body, so the backends agree by construction; the slot table owns every
//! reference the loops juggle, so the early-exit paths cannot leak.

use noeta_native::registry::{ExtFn, RetTy, SigType};
use noeta_native::{
    CtxError, CtxOut, CtxResult, ErrorKind, NativeCtx, NativeValue, Scalar, Slot, StdError,
    panic_error, type_error,
};

const VAR_A: SigType = SigType::Var(0);
const VAR_B: SigType = SigType::Var(1);
const FUT_A: SigType = SigType::Future(&VAR_A);
const FUT_B: SigType = SigType::Future(&VAR_B);

pub const TASK_CTX_FNS: &[ExtFn] = &[
    // `sleep(ms) -> Future<void>` (Track A.2): a leaf timer future, ready once the executor's
    // clock reaches `now + ms`.
    ExtFn {
        name: "sleep",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Future(&SigType::Unit)),
    },
    // `all(List<Future<A>>) -> List<A>` (Track A.9): await every future concurrently, results in
    // list order.
    ExtFn {
        name: "all",
        params: &[SigType::List(&FUT_A)],
        ret: RetTy::Concrete(SigType::List(&VAR_A)),
    },
    // `race(List<Future<A>>) -> A` (Track A.9 + A.8): first ready result wins, losers cancelled;
    // ties break deterministically by list order.
    ExtFn {
        name: "race",
        params: &[SigType::List(&FUT_A)],
        ret: RetTy::Concrete(VAR_A),
    },
    // `map_bounded(List<A>, int, Fn(A) -> Future<B>) -> List<B>` (Track A.9): apply async `f` to
    // each item with at most `n` futures in flight, results in item order.
    ExtFn {
        name: "map_bounded",
        params: &[
            SigType::List(&VAR_A),
            SigType::Int,
            SigType::Fn(&[VAR_A], &FUT_B),
        ],
        ret: RetTy::Concrete(SigType::List(&VAR_B)),
    },
];

/// Validate a list argument with the migrated builtins' own message shape (`` `all` expects a
/// list of futures, found int ``) and hand back its length.
fn expect_list(
    ctx: &mut dyn NativeCtx,
    func: &str,
    slot: Slot,
    expected: &str,
) -> CtxResult<usize> {
    if !ctx.is_list(slot)? {
        return Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!(
                "`{func}` expects {expected}, found {}",
                ctx.type_name(slot)?
            ),
        }
        .into());
    }
    ctx.list_len(slot)
}

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
        // `all(list)`: poll every not-yet-ready element each round; between rounds give the open
        // `concurrent` scopes a round, then advance the clock — and when all three stall, wait
        // for an external (isolate) wake or report the deadlock.
        "all" => {
            noeta_native::ctx_arity(func, args, 1)?;
            let n = expect_list(ctx, func, args[0], "a list of futures")?;
            let futures = list_slots(ctx, args[0], n)?;
            let mut results: Vec<Option<Slot>> = vec![None; n];
            loop {
                let wake_gen = ctx.wake_generation();
                for i in 0..n {
                    if results[i].is_none() {
                        results[i] = ctx.poll(futures[i])?;
                    }
                }
                if results.iter().all(Option::is_some) {
                    let ready: Vec<Slot> =
                        results.into_iter().map(|r| r.expect("all ready")).collect();
                    return Ok(CtxOut::Slot(ctx.make_list(&ready)?));
                }
                let progressed = ctx.advance_tasks()?;
                if !progressed && ctx.advance_clock().is_none() && !ctx.wait_external_wake(wake_gen)
                {
                    return Err(panic_error(
                        "async deadlock: `all` awaited futures with no pending timers",
                    )
                    .into());
                }
            }
        }
        // `race(list)`: the first ready result returns; every other entry's task is cancelled
        // (cooperative — a loser never resumes past its last suspension).
        "race" => {
            noeta_native::ctx_arity(func, args, 1)?;
            let n = expect_list(ctx, func, args[0], "a list of futures")?;
            if n == 0 {
                return Err(panic_error("`race` requires at least one future").into());
            }
            let futures = list_slots(ctx, args[0], n)?;
            loop {
                let wake_gen = ctx.wake_generation();
                for i in 0..n {
                    if let Some(winner) = ctx.poll(futures[i])? {
                        for (j, &loser) in futures.iter().enumerate() {
                            if j != i {
                                ctx.cancel(loser)?;
                            }
                        }
                        return Ok(CtxOut::Slot(winner));
                    }
                }
                let progressed = ctx.advance_tasks()?;
                if !progressed && ctx.advance_clock().is_none() && !ctx.wait_external_wake(wake_gen)
                {
                    return Err(panic_error(
                        "async deadlock: `race` awaited futures with no pending timers",
                    )
                    .into());
                }
            }
        }
        // `map_bounded(items, n, f)`: a sliding window — top up to `n` in-flight futures (each
        // `ctx.call` of `f` starts the async body up to its first suspension), poll them, collect
        // completions in item order, repeat. Freeing each item and resolved future keeps the
        // table bounded on long lists.
        "map_bounded" => {
            noeta_native::ctx_arity(func, args, 3)?;
            let count = expect_list(ctx, func, args[0], "a list")?;
            let NativeValue::Scalar(Scalar::Int(limit)) = ctx.view(args[1])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!(
                        "`map_bounded` expects an int concurrency limit, found {}",
                        ctx.type_name(args[1])?
                    ),
                }
                .into());
            };
            let f = args[2];
            let window = limit.max(1) as usize;
            let mut results: Vec<Option<Slot>> = vec![None; count];
            let mut in_flight: Vec<(usize, Slot)> = Vec::new();
            let mut next = 0usize;
            let mut done = 0usize;
            loop {
                let wake_gen = ctx.wake_generation();
                while in_flight.len() < window && next < count {
                    // Fused element call: no per-item slot is minted (see `call_with_element`).
                    let future = ctx.call_with_element(f, args[0], next)?;
                    in_flight.push((next, future));
                    next += 1;
                }
                if done == count {
                    let ready: Vec<Slot> =
                        results.into_iter().map(|r| r.expect("all done")).collect();
                    return Ok(CtxOut::Slot(ctx.make_list(&ready)?));
                }
                let mut progressed = false;
                let mut k = 0;
                while k < in_flight.len() {
                    let (index, future) = in_flight[k];
                    // A `Some` poll spends the future's slot (the result reuses its index).
                    if let Some(result) = ctx.poll(future)? {
                        results[index] = Some(result);
                        in_flight.remove(k);
                        done += 1;
                        progressed = true;
                    } else {
                        k += 1;
                    }
                }
                progressed |= ctx.advance_tasks()?;
                if !progressed && ctx.advance_clock().is_none() && !ctx.wait_external_wake(wake_gen)
                {
                    return Err(panic_error(
                        "async deadlock: `map_bounded` stalled with no pending timers",
                    )
                    .into());
                }
            }
        }
        _ => Err(noeta_native::no_function_error("task", func).into()),
    }
}

/// Mint a slot per element of a list argument (the combinators poll the same handles round after
/// round, so fetch each once).
fn list_slots(ctx: &mut dyn NativeCtx, list: Slot, n: usize) -> CtxResult<Vec<Slot>> {
    (0..n).map(|i| ctx.list_get(list, i)).collect()
}
