# Interruptible host IO — design proposal

Status: proposal (not started). The **wake seam** it depends on shipped in the `interruptible-io` slice; everything below is the consumer side.

This is the residual of the backlog's "Interruptible host IO" row, written after measuring the ground it stands on. It exists as a plan rather than a slice because the measurement changed the shape of the answer twice, and both changes are worth recording before anyone writes code.

## The question the row asked: do the two cancellation holes share a seam?

They share a **token**, not a mechanism, and the split is clean.

*Shared.* Both holes are the same sentence: a worker that is blocked outside the interpreter reaches no safepoint, so setting its cancellation flag is observed only when the block ends. Both therefore need one thing from the canceller — a way to say "stop" that also *rouses* a party that is not looking. That is `CancelWake` (`noeta-ext-abi`, beside `Executor`): a hook list the canceller fires straight after the flag store, on which anything that can block registers whatever its own blocking primitive understands. It is deliberately hook-shaped rather than a channel or a `Notify` precisely so a second, unrelated consumer can register on it; the executor is the first, a host leaf would be the second, and `noeta-vm` — which owns the cancel — needs to know about neither.

*Not shared.* The consumers are not alike, in two ways that matter.

1. **Arity.** Hole 2 has exactly one consumer: `RealExecutor::advance`, whose two blocking points are one `select!` away from being interruptible. Hole 1 has one consumer per blocking leaf, and they do not block on the same primitive (inventory below).
2. **What "interrupted" means.** Ending the executor's *wait* is a complete fix for a timer: the worker wanted nothing but elapsed time, so returning early loses nothing. Ending a *read's* wait is not a fix for the read, because the read still has to say what happened. Every blocking leaf needs an `Interrupted` outcome in its own vocabulary, and the leaves differ on whether that outcome is even reachable (again, below).

So: build the token once — done — and treat the interruption of host IO as its own change. That is what happened.

## What shipped (context for the rest)

`CancelWake` + `Executor::set_cancel_wake` (a no-op default, so `SandboxExecutor` and the oracle are untouched), a per-executor `tokio::sync::Notify` in `RealExecutor` selected on at every point `advance` can block, and the wake handle created by the parent at spawn beside the flag and armed by the worker on its own executor before any user code runs. `request_isolate_cancel` is now `store(true)` then `wake()`.

Measured: a worker in `sleep(3000).await` cancelled 200 ms in ends the run at **0.21 s**, against **3.01 s** with the wake ablated on the same binary.

## Finding 1 — the row's own example is not blocked in a syscall

The row names `Process.read_line` first, and sketches "register the fd with a self-pipe/`eventfd` the cancel writes". That is not what the code does. `RealHost` drains a child's stdout and stderr on their own threads into a `SharedStream`, and `read_line`/`read` block on a **`std::sync::Condvar`** waiting for that buffer to grow (`noeta-host-real/src/lib.rs`, `SharedStream::read_line` / `SharedStream::read`). There is no fd to register: the read is interruptible with a flag check and a `notify_all`, in our own code, with no platform machinery at all.

That makes the cheapest, most-wanted third of hole 1 far cheaper than the row assumed — and it means the eventfd design should not be adopted wholesale. The inventory decides per leaf.

## Finding 2 — ending the wait is not ending the work

This is the one that changes the design, and it is now pinned by a test (`noeta-host-real`'s `a_started_blocking_body_outlives_the_executor_it_was_spawned_on`).

A blocking leaf's real body is a `spawn_blocking` closure on the isolate's own tokio runtime. Dropping a runtime waits for every blocking task that has already started. So a worker that is woken out of its wait, unwinds, and tears down **blocks again in teardown** until the leaf returns — and a leaf that never returns holds the worker, and therefore the `concurrent` block joining it, indefinitely.

Measured both ways:

- Unit: a 2 s blocking body is woken past in 50 ms, and then costs the executor's drop the remaining 1.95 s.
- CLI: a worker awaiting `fs.read_async` on a FIFO with no writer, cancelled at 300 ms, hangs past 20 s. Unwedging the FIFO at 900 ms ends the run at 903 ms.

The consequence is a hard requirement on any design: **the leaf must return, not be abandoned.** "Drop the future and move on" leaves the work running, the runtime waiting for it, and the structured-concurrency guarantee — which we are not willing to give up, since abandoning a worker segfaulted the allocator reproducibly — turns the leak into a hang. `Runtime::shutdown_background` would trade the hang for exactly the abandonment we refuse.

## Inventory of blocking leaves (`noeta-host-real`)

| Leaf | Blocks on | Interruptible? | Cost |
|---|---|---|---|
| `os_proc_read_line` / `os_proc_read` / `os_proc_read_err_line` | `SharedStream`'s own `Condvar` (`lib.rs` ~1314/1345) | **Yes, cheaply.** Wake = `notify_all`; the loop re-checks a flag and returns `Interrupted` | one flag + one hook per host |
| `os_proc_wait` (sync) | `std::process::Child::wait` — a real `waitpid` | Not directly. Needs the existing `wait_async` machinery (a waiter thread + the `slot.ready` condvar at ~1610/1849, already condvar-shaped) or a `try_wait` poll loop | medium |
| `fs_read` / `fs_write` / … (sync) | `runtime.block_on(tokio::fs::…)`, i.e. the blocking pool | Only by making the *operation* interruptible: chunked reads with a flag check between chunks, or `O_NONBLOCK` + poll for FIFOs/character devices. Dropping the future does not stop the read (Finding 2) | high |
| `fs.*_async` (the awaited twins) | same blocking pool, via the executor's `JoinSet` | The *wait* is already interruptible (shipped); the work is not (Finding 2) | high |
| `net_fetch` (sync http) | `runtime.block_on(reqwest…)` — a genuine future | **Yes.** `select!` the future against the wake and return `Interrupted`; dropping a reqwest future really does cancel the request | low |
| streaming bodies (`stream.rs` ~176/328) | `std::sync::mpsc::Receiver::recv` on the pump thread's channel | Yes, via `recv_timeout` + flag, or a sentinel frame pushed by the wake hook | low |
| websocket recv | already async on the executor | wait interruptible (shipped); same work caveat | — |

Two structural notes on that table. `os_proc_*` and the streams are **our own** synchronization, so they are cheap and complete. `fs`/`net` are foreign work on a pool, where the only honest interruption is one the operation itself supports.

## Proposed design

**1. Give the `Host` the token, exactly as the executor got it.** A `Host::set_cancel(&mut self, flag: Arc<AtomicBool>, wake: Arc<CancelWake>)` with a no-op default (so `SandboxHost` is untouched and the oracle stays deterministic and in-oracle), called by `run_isolate_worker` beside `executor.set_cancel_wake`, from the same pair it already holds. `RealHost` stores the flag and registers a hook that fans out to its own primitives: `notify_all` on every live `SharedStream`, a sentinel on every open stream channel, `notify_all` on the `wait_async` slots.

**2. One new error kind: `Interrupted`.** A new `ErrorKind` in `noeta-stdlib` (with its diagnostic code) that every interruptible leaf returns when the flag is set. It is *not* a user-facing failure mode in normal operation: the worker's very next safepoint turns it into the ordinary cancellation unwind. But it must be a real error rather than a silent `none`, because a `read_line` that answers `none` means "end of stream", and a cancelled read is not an end of stream. Note this is the one place the design leaks into the sandbox's surface (a new variant), so it wants a corpus case pinning that the sandbox never produces it.

**3. Slice order, cheapest and most-wanted first.**

- **H1 — process reads.** `SharedStream::read_line`/`read` take the flag, wait on `wait_timeout` (or plain `wait` plus the hook's `notify_all`), and return `Interrupted`. Closes the row's headline example. Test: a worker blocked on a child that never speaks, cancelled, stops promptly — using the isolates house rule (a child that *never* writes, so cancellation is the only exit).
- **H2 — sync http + streaming.** `select!` the reqwest future against the wake; `recv_timeout` the stream channels. Both are small and both are shapes a real agent app meets (`para/ai`'s transports).
- **H3 — `os_proc_wait`.** Route the sync wait through the existing waiter-thread + condvar slot so it shares H1's hook.
- **H4 — fs.** The expensive one, and the only one that needs a decision rather than an implementation (below). Do it last, or not at all.

**4. The `fs` decision, stated rather than deferred.** There are three defensible answers and the choice should be explicit:

- *Leave it.* A blocking file read that never returns is a FIFO or a character device, not a file; the documented answer stays "put a deadline on the operation". Cheapest, and honest, but leaves the row's "blocking syscall" third open forever.
- *Chunk it.* Replace `tokio::fs::read_to_string` with a loop that reads a bounded chunk and re-checks the flag. This genuinely interrupts, costs a little throughput on large files, and does not help a read that blocks on the *first* chunk — which is exactly the FIFO case, so it fixes less than it looks.
- *Open non-blocking.* For paths that are not regular files, open with `O_NONBLOCK` and poll. This is the only option that closes the FIFO case, and it is the eventfd-shaped, platform-specific work the original row imagined. Unix-only; Windows needs its own story.

Recommendation: ship H1–H3, take the *leave it* answer for H4 with the reasoning written into `docs/Concurrency.md`, and let a real consumer's need pick between chunking and non-blocking opens later.

## Adjacent, and cheap once someone is in here

The **top-level** cancellable run (`RunOptions::cancel` — the `noeta test` per-case deadline) has hole 2 in its own form: a case parked in a long `sleep` is not woken, so `stop_overrun_case` waits out its 1 s grace and then *abandons* the case's thread, leaking it and its isolate. The fix is the same wake, and the only reason it is not in the shipped slice is plumbing: `cancel` travels as a bare `Arc<AtomicBool>` through `run_one_test` → `execute_real_host` → `run_module_real_host` → `noeta-runner` → `RunOptions`, so a second parameter means five signatures across four crates.

The better version of that change is to stop having two objects: fold the flag and the wake into one `CancelSignal { flag: AtomicBool, wake: CancelWake }` and make `noeta_vm::CancelFlag` an `Arc<CancelSignal>`. Every existing `cancel` parameter then carries the wake for free, `RunOptions` grows nothing, and the CLI's `stop_overrun_case` becomes `cancel.request()`. The cost is that the JIT bakes the flag's address as an immediate (`noeta-jit`'s `cancel_flag` + `jit_service`), so it would bake `&signal.flag` instead and needs `--jit-differential --cancel-poll` re-run as its gate. That is a contained change, but it is a JIT change, which is why it was not folded into a slice whose gates do not include the JIT oracle.
