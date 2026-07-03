# S2 — Register stack (P-VMT-FRAME)

**Goal.** Stop heap-allocating a register file on every function call. Today each call does
`vec![Value::unit(); num_registers]` (a fresh `Vec` alloc + later drop); fib(32) ≈ 7M calls = 7M
alloc/free pairs.

## Evidence

Ablation: a bare `id(i)` call costs **~87 ns/iter** on top of the loop. fib(32) is 669 ms vs PHP 48 ms
(28×) — dominated by call overhead. The allocation sites (all `vec![Value::unit(); chunk.num_registers]`):

```
crates/lang-vm/src/lib.rs: 561, 894, 1433, 1563, 1725, 1775, 2159, …
```

`Frame` is 80 bytes and owns `regs: Vec<Value>` (24 B) + `upvalues: Vec<…>` (24 B) — two heap
allocations per frame in the worst case.

## Approach

Replace per-frame `Vec<Value>` register files with **windows into one contiguous register stack** held
by the VM for the whole run:

1. VM owns `regs: Vec<Value>` (a stack) plus a `base` cursor. A frame records `base` and
   `num_registers`; its registers are `regs[base .. base + n]`.
2. **Call:** reserve `n` slots by extending the stack (amortized O(1), no per-call alloc after warm-up
   — the stack grows to the deepest recursion once and is reused). Push a `Frame { base, … }`.
3. **Return:** truncate the stack back to the caller's top; move the return value into the caller's
   `ret_dst` before truncating.
4. Register access becomes `regs[base + i]` — one indexing instead of `frames[top].regs[i]` (two).
   This directly enables S3 (the window is already the shape S3 hoists into a local).

Keep `upvalues` handling as-is for now (closures are rarer than calls); a follow-up can pool them.

## Files

- `crates/lang-vm/src/lib.rs` — `Frame` struct, `dispatch` call/return arms, every register-file
  allocation site, `set_reg`/`get` helpers to take `(regs, base, i)`.

## Validation

- **Benchmark:** criterion `vm.rs` — a recursion bench (fib(30), ackermann, or deep mutual recursion)
  and a flat call-in-loop bench. Target: eliminate the per-call allocation (visible as a large drop in
  the recursion bench and in allocations via `lang-alloc-probe`).
- **Oracle:** pure representation change, invisible to `RunResult` → differential `0 skipped / agree`.
  Existing conformance (recursion, closures, deep call chains) is the correctness net; add a
  deep-recursion case if coverage is thin.
- Miri clean (the register stack is safe `Vec` indexing; no new `unsafe`).

## Risk

Medium. The subtlety is **stack-relocation invalidation**: growing `regs` may reallocate, so no code
may hold a `&mut` into `regs` across a push. Access by `(base, index)` each time (not by borrowed
slice held across a call) avoids it. Return-value move must happen before truncation. Closures/upvalues
that capture locals already copy `Value`s (refcounted), so windows don't alias captured state.

## Dependencies

None to start, but **S3 builds directly on this** — do S2 first so S3's dispatch window wraps the
final register representation.
