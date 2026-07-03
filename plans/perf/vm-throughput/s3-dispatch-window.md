# S3 — Dispatch register window (P-VMT-DISP)

**Goal.** Lower the per-instruction floor. An empty loop runs at **80 ns/iter** (PHP ≈ 3.6) — before
any real work — because the dispatch loop re-derives the current frame and re-bounds-checks every
access on every op.

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
