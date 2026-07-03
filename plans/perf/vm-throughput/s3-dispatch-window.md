# S3 — Dispatch register window (P-VMT-DISP)

**Status: DONE.** The dispatch loop now hoists the active frame's register `base`, its prototype
(`chunk`), and its `pc` into loop-locals that are re-derived **only** on a control transfer — a call
pushes a frame, a return / short-circuiting `?` pops one — through a `reload!(…)` macro. A
straight-line op is now just `pc += 1` on the local; a jump assigns the local; neither re-indexes
`frames` nor re-bounds-checks the prototype table, and `chunk.code.get(pc)` (an `Option` +
bounds-check) became a direct `chunk.code[pc]` (every prototype ends in `Halt`, so `pc` is always in
bounds). All safe indexing — **no new `unsafe`** (the all-safe hoist already captured the win; a
`get_unchecked` pass was not needed).

Result (release, before = the S2 contiguous-register-stack commit, same session):

| bench | S2 (before) | S3 (after) | speedup |
|--|--:|--:|--:|
| `loop_sum` 1,000,000 iters — the dispatch floor | 63.0 ms (~63 ns/iter) | 43.1 ms (~43 ns/iter) | **1.46×** |
| `loop_sum` 100,000 iters | 5.34 ms | 3.98 ms | 1.34× |
| `fib(28)` | 104.9 ms | 93.2 ms | 1.13× |
| `fib(24)` | 15.28 ms | 13.83 ms | 1.10× |
| `fib(20)` | 2.23 ms | 1.99 ms | 1.12× |

The tight arithmetic loop (no per-iteration heap work) is where the win concentrates: the
per-instruction floor drops ~32% (63 → 43 ns/iter). Call-heavy recursion gains less (~11%) because a
call's own work — argument marshalling, `retain`/`release`, window reservation — dominates the
dispatch savings there; those are what S4 (`Op` shrinking) and later slices target.

Behaviour-neutral (pure execution-structure change): differential 419 / 0 skipped / backends agree,
corpus 430 passed, leak oracle residency 0 on both backends, 118 VM unit tests, clippy clean, miri
unaffected (no `unsafe` added). Criterion bench `vm_dispatch/loop_sum/{100000,1000000}` added.

**Design note — why a single loop, not a nested one.** The obvious shape for a reload point is an
outer `'reload: loop { … inner loop … }` with `continue 'reload` on transfer. That works but forces
the entire ~2700-line match body to re-indent by one level (a huge, noise-only diff). The single-loop
form keeps `top`/`fbase`/`chunk`/`pc` as `let mut` before the loop and re-derives them via `reload!()`
exactly at the 12 transfer points (9 call-push sites, the `Return`/`Halt` arms, and the `?`
short-circuit). Every other op falls straight through the match and loops with the locals it already
mutated — no label, no re-indent. Because `chunk: &'m Proto` points into `*module` (an `&'m Module`
copied out of `self`), reassigning the `chunk` local mid-loop is sound: the reference it yields is
tied to the module's lifetime, not to the variable's storage, so `&mut self` calls in the arms never
conflict. Local `macro_rules!` is hygienic, so the mutated locals (and `frames`/`module`) are passed
as macro arguments to resolve in the caller's scope.

**Original goal.** Lower the per-instruction floor. An empty loop ran at **80 ns/iter** (PHP ≈ 3.6) —
before any real work — because the dispatch loop re-derived the current frame and re-bounds-checked
every access on every op.

## Evidence

The loop head (`crates/lang-vm/src/lib.rs` ~959) redoes, per instruction:

```rust
let top = frames.len() - 1;                         // recompute
let chunk = &module.protos[frames[top].proto as usize];  // index protos (bounds-checked)
let pc = frames[top].pc;                            // index frames
let Some(op) = chunk.code.get(pc) else { … };       // Option + bounds check
match op { … }                                       // 84-arm
// then per operand:  frames[top].regs[i]            // frames[] + regs[] = 2 indexings, 2 bounds checks
```

Ablation empty loop = 80 ns/iter for ~8 ops ≈ 10 ns/op — an order of magnitude off a tuned register
VM (~1–3 ns/op).

## Approach

Rewrite the dispatch loop around a **current-frame window** made cheap by S2's contiguous register
stack:

1. Hoist the active frame's `base` (into the register stack), `chunk` reference, and `pc` into loop
   locals. Re-sync them **only** on the events that change them — `Call`, `CallMethod`, `Return`,
   tail-call — not on every op. Most ops just `pc += 1` on the local.
2. Register access becomes `regs[base + i]`; where the borrow checker allows, take the current frame's
   register slice once per op-group. Prefer `get_unchecked` **only** behind a verified-bounds
   invariant (the compiler's register allocator fixes `num_registers` ≥ every index it emits) — gate
   any `unsafe` behind a debug-assert and keep it isolated (this crate is otherwise safe; `lang-value`
   owns the one `unsafe`). If we'd rather stay 100% safe, the win from hoisting `base`/`chunk`/`pc`
   alone (removing the repeated `frames[top]` indirection) is most of it — measure both.
3. Replace `chunk.code.get(pc)` with an indexing on the hoisted `chunk.code` (every proto ends in
   `Halt`, so pc never runs off the end — the `.get` guard is redundant on the hot path).

## Files

- `crates/lang-vm/src/lib.rs` — the `dispatch` loop structure; touches every op arm's register access
  and `pc`/frame handling (mechanical but broad).

## Validation

- **Benchmark:** criterion `vm.rs` dispatch bench (the existing M2.0 dispatch baseline) — empty loop
  and arithmetic loop. Target: cut the 80 ns/iter floor toward single-digit ns. Record before/after.
- **Oracle:** pure execution-structure change, invisible to `RunResult` → differential `0 skipped /
  agree`. The full corpus is the correctness net (all control flow flows through this loop).
- Miri clean; if any `get_unchecked` is introduced, miri **must** still pass (it validates the
  bounds invariant).

## Risk

Medium–high (broad edit). The hazards: frame-local `base`/`chunk`/`pc` must be re-synced on **every**
control-transfer arm (a missed re-sync = executing the wrong proto — caught immediately by the corpus,
but easy to introduce). Any `unsafe` indexing must be provably in-bounds and miri-clean; start with the
all-safe hoist and add `get_unchecked` only if the bench justifies it.

## Dependencies

**S2** (register stack) — S3's window is a view into S2's contiguous `regs`. Doing S2 first means S3
is written once against the final representation. Co-schedule with **S4** (both touch the hot loop; S4
shrinks the `Op` the loop streams).
