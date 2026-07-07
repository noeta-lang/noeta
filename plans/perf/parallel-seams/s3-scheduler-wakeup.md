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

## Shipped design (differs from the sketch above in one way)

A **process-wide eventcount** (`isolate::WAKE`: `AtomicU64` generation + `Mutex`/`Condvar`),
not a per-parent signal — one event source (a shared `ChannelCore`) can unblock schedulers in
several isolate trees, and registration plumbing isn't worth it when a spurious wake just
re-polls. Every round loop (join_scope, drive_future, `all`/`race`/`map_bounded`) snapshots the
generation **before** polling and parks in `wait_past(seen, 5ms)` only if it hasn't moved —
check-then-wait under the condvar lock, so no missed-wakeup window. Notify points: worker
result landing (`try_spawn_isolate_real` closure), `ChannelCore::{try_send, try_recv-Got,
close}`. Sandbox: never parks (no cross-thread pending); pays one atomic load per round.

## Numbers

2026-07-07, same harness as S0b (`tests/bench/parallel-seams/run.py`, median of 7).

| Fixture | before (S0b) | after | Δ |
|---|--:|--:|--:|
| `pingpong` (2000 cross-thread rounds) | 319.3 ms | **15.5 ms** | **20.6× faster** (~160 → ~7.8 µs/round) |
| `pingpong_coop` (floor) | 13.6 ms | 14.7 ms | unchanged (noise) |

The cross-thread ping-pong now sits ~1 ms above the single-thread cooperative floor. CPU/run
23 ms (vs 24 ms before): the old cost was pure sleep, and the wakeups add nothing measurable.
Fan-out fixtures unchanged (marshal-dominated; S2's target).
