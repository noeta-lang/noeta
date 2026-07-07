# S3 — P-PAR-WAKE: park/wakeup instead of the 100 µs sleep-spin

## Today

When the parent scheduler stalls with cross-thread work outstanding, `isolate_in_flight_wait`
(`crates/noeta-vm/src/scheduler.rs:71-84`) sleeps 100 µs and re-polls, forever. Every
producer/consumer round over a shared channel eats ~½ quantum of dead time (S0b measures it),
and an idle parent wakes 10k times/second for nothing. The channel core itself
(`crates/noeta-vm/src/isolate.rs:28-64`, `Arc<Mutex<ChannelInner>>`) is documented "no
`Condvar` — cooperative poll", which is right in-oracle but leaves the real path spinning.

## The change

One shared wakeup event, not a per-channel condvar (the parent waits on *any* progress, not one
channel):

- A `WakeSignal` (`Arc<(Mutex<u64>, Condvar)>` generation counter, or an eventcount built on
  `park`/`unpark`) owned by the parent scheduler and cloned into (a) each real-isolate worker's
  completion path (after `tx.send(msg)` in the spawn closure, `scheduler.rs:407-410`) and
  (b) `ChannelCore` shared send/recv (`isolate.rs`), signalled after any successful cross-thread
  `try_send`/`try_recv`/close.
- `isolate_in_flight_wait` reads the generation, re-checks `cross_thread_pending`, then waits on
  the condvar **with a timeout** (e.g. 1–5 ms) as a belt-and-braces fallback so a lost-wakeup
  bug degrades to today's behaviour instead of a hang. Spurious wakeups are harmless (the caller
  loops and re-polls by design).
- Check-then-wait ordering (read generation → poll → wait-if-unchanged) closes the classic race
  where the worker signals between the parent's poll and its sleep.

## Not touched

- The deterministic sandbox never reaches this function with `cross_thread_pending` (no real
  isolates, all channels `Local`), so cooperative deadlock detection is byte-identical.
- Channel *ops* stay non-blocking `try_*` under the short lock; only the parent's stall wait
  changes. Workers already block only in `run_isolate_worker`'s own scheduler, which gets the
  same wakeup via its cloned signal if it has shared channels.

## Gate

- **S0b re-run:** ping-pong round latency drops from quantum-bound (~50–100 µs floor) to
  wakeup-bound; idle parent CPU-time/wall-time ratio collapses.
- No deadlock-detection regression: the deterministic deadlock conformance cases and the
  timeout fallback path both exercised.

## Numbers

_Before (S0b) / after table to be recorded here._
