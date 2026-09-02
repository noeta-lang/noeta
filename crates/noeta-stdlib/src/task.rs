//! The `task` concurrency module: the async combinators, registered
//! through the higher-order **ctx** dispatch table because they need the executor (and, for the
//! combinators, closure call-backs) — capabilities the plain value-in/value-out registry seam
//! deliberately does not carry.
//!
//! `sleep` was the seam's proving client; `all`/`race`/`map_bounded` are the first
//! genuine orchestrators — they call closures back, poll many futures, drive the scheduler — and
//! their loops here are line-for-line the drive loops the deleted per-backend `Builtin` arms
//! duplicated. One shared body, so the backends agree by construction; the slot table owns every
//! reference the loops juggle, so the early-exit paths cannot leak.

use noeta_ext_abi::registry::{ExtFn, RetTy, SigType};
use noeta_ext_abi::{
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
        param_names: &["ms"],
        name: "sleep",
        params: &[SigType::Int],
        ret: RetTy::Concrete(SigType::Future(&SigType::Unit)),
    },
    // `all(List<Future<A>>) -> List<A>` (Track A.9): await every future concurrently, results in
    // list order.
    ExtFn {
        param_names: &["futures"],
        name: "all",
        params: &[SigType::List(&FUT_A)],
        ret: RetTy::Concrete(SigType::List(&VAR_A)),
    },
    // `race(List<Future<A>>) -> A` (Track A.9 + A.8): first ready result wins, losers cancelled;
    // ties break deterministically by list order — see the dispatch arm for what counts as a tie.
    ExtFn {
        param_names: &["futures"],
        name: "race",
        params: &[SigType::List(&FUT_A)],
        ret: RetTy::Concrete(VAR_A),
    },
    // `map_bounded(List<A>, int, Fn(A) -> Future<B>) -> List<B>` (Track A.9): apply async `f` to
    // each item with at most `n` futures in flight, results in item order.
    ExtFn {
        param_names: &["items", "limit", "f"],
        name: "map_bounded",
        params: &[
            SigType::List(&VAR_A),
            SigType::Int,
            SigType::Fn(&[VAR_A], &FUT_B),
        ],
        ret: RetTy::Concrete(SigType::List(&VAR_B)),
    },
];

/// Validate a list argument with the builtins' own message shape (`` `all` expects a
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
            noeta_ext_abi::ctx_arity(func, args, 1)?;
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
            noeta_ext_abi::ctx_arity(func, args, 1)?;
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
        // `race(list)`: the first ready result returns; every other entry's task is cancelled.
        // `race` itself does not wait for the losers to stop — it hands the winner back at once —
        // but the enclosing `concurrent` block's closing brace does, which is where a real-isolate
        // loser is joined after it honors the request at its next safepoint (isolate-cancel).
        //
        // **What counts as a tie**, since this is the arm people read that question into: the loop
        // below scans the list in order each round, so the winner is the earliest LIST position
        // among everything ready in the round that first produced anything. That is the whole rule,
        // and it is a rule about readiness rather than completion because readiness is all a poll
        // can observe — arbitrary real time may pass between two rounds (a preempted thread, a
        // blocking host call, a collection), and every future that became ready inside that window
        // did so unobserved. Recording a completion timestamp would not recover an ordering: a
        // task's "completion" *is* the round in which the scheduler resumed it, and the scheduler
        // resumes in spawn order, so the timestamp would record this tie-break rather than discover
        // anything. Only a pure timer leaf carries an earlier fact (its deadline), and preferring it
        // would give one future kind a rule the others cannot have.
        //
        // Under the sandbox executor the tie is unreachable for timers: `advance` *jumps* logical
        // time to exactly the next deadline, so only the earliest-deadline timer is ever due at a
        // poll. The real executor sleeps real time to that deadline and wakes late, so every
        // deadline the overshoot crossed comes due together — which is why a corpus case whose
        // answer is a timer ordering states it in gaps a loaded scheduler cannot close, and why
        // `tests/conformance/async/race_tie_list_order.noe` pins the tie itself with futures that
        // never suspend at all.
        "race" => {
            noeta_ext_abi::ctx_arity(func, args, 1)?;
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
            noeta_ext_abi::ctx_arity(func, args, 3)?;
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
        _ => Err(noeta_ext_abi::no_function_error("task", func).into()),
    }
}

/// Mint a slot per element of a list argument (the combinators poll the same handles round after
/// round, so fetch each once).
fn list_slots(ctx: &mut dyn NativeCtx, list: Slot, n: usize) -> CtxResult<Vec<Slot>> {
    (0..n).map(|i| ctx.list_get(list, i)).collect()
}
