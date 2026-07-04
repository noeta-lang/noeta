# P-CALL — native call frames (inline the call/return sequence into JIT code)

Status: **proposal for sign-off.** Milestone-scale (multi-slice), out-of-oracle (JIT is real-host
only), gated end-to-end by `--jit-differential` (byte-identical `RunResult` + leak-under-JIT residency 0).

## Why

The 2026-07-04 cross-language benchmark put Noeta's function calls last of the field: fib(32) at
**~33 ns/call** (JIT) vs LuaJIT ~2, PHP-JIT ~4, CPython ~22. The [`p-direct-calls`](../../..) pass
(`Op::CallGlobal`, A+B) removed the per-call global-load + closure refcount churn and bought **~5%**
— confirming, by how little it moved, that the global load was *not* the bottleneck.

The bottleneck is the **per-call frame-setup helper round-trip**. A native `Op::Call`/`CallGlobal`
today emits calls to the runtime helpers `jit_prepare_call` / `jit_call` / `jit_after_call` (in
`noeta-vm`). Each helper reconstitutes `&mut Vm` from the ABI pointer, then reserves a register
window and pushes/pops a `Frame` on the shared `Vec<Frame>` / `Vec<Value>` stacks. That helper call
+ its work is most of the ~27 ns/call of pure call overhead (the arithmetic per fib call is ~6 ns,
matching a loop iteration). J3's direct native→native path already skips the *interpreter* round-trip
between compiled caller and callee — but both still go *through* the setup helper.

**Goal:** emit the reserve-window / write-args / push-Frame / call / pop-Frame / transfer-result
sequence as native machine code, with **no helper call on the hot path** — the call analogue of what
J1/J2 did for arithmetic (native ops, helpers only for the cold/heap cases).

## The prerequisite (the hard part) — a layout-lockable frame representation

The JIT cannot today write `Frame` fields or grow `Vec<Frame>` in native code: `Frame` is `#[repr(Rust)]`
(unstable field offsets) and `Vec`'s header layout is not guaranteed. This is the **same class of
blocker** the native inline cache hit (J6) and is solved the **same proven way** — not by assuming a
layout, but by **measuring** it in-process and baking the measured offsets into code generated in the
same process (so it cannot drift), with a lock test that fails the build if the probe stops locating
the real fields. See `lang_value::object_layout()` (P-JIT J6 / object-model) for the pattern.

Concretely, a new `noeta_vm::frame_layout()` / `stack_layout()` returns:
- `Frame` field offsets: `base` (usize), `proto` (u32), `pc` (usize), `ret_dst` (u16), and the
  `upvalues: Vec<Value>` field (native path only sets it empty for the plain-fn shape — the B fast
  path — and bails to the helper for closures with upvalues).
- `Vec<Frame>` / `Vec<Value>` header layout (ptr / len / cap word offsets) so native code can push
  by writing at `ptr + len*stride` and bumping `len`, **guarded on `len < cap`** (a realloc would
  dangle the caller's register pointer → bail to the helper, exactly as `jit_prepare_call` already
  guards capacity today).

## Slices (each ends green on `--jit-differential`, with a fib before/after)

- **S1 — lock the layout (behaviour-neutral).** `frame_layout()`/`stack_layout()` + lock tests. No
  codegen change. Gate: differential 431/0, jit-differential 431/0 unchanged, miri-clean.
- **S2 — native window + args.** Emit the register-window reserve (capacity-guarded) and the arg
  copy (with the heap-aware retain where a param may be heap) inline; still call the helper for the
  Frame push. Isolates the cheap half. Measure fib.
- **S3 — native Frame push/pop (the payoff).** Emit the `Frame` write at measured offsets + `len`
  bump for the plain-fn shape; on return, native pop + `len` decrement + window truncate. Helper
  remains the fallback for: closures with upvalues, default-filling arity, capacity overflow,
  non-closure callees. Measure fib — this is where the number moves.
- **S4 — native return transfer.** Inline the value-returning `Return` → caller-dst transfer
  (extends J3's `jit_return`). Close the loop so a fib frame runs fully native, helper-free, except
  the cold fallbacks.

## Refcount + soundness posture (non-negotiable)

Every native store stays refcount-exact via the existing heap-aware discipline (the shared register
stack makes the tier boundary free — P-VMT-FRAME's payoff). Bail-before-mutate holds: any guard
(capacity, upvalue-count, arity) decides *before* touching the stack so the interpreter re-runs the
op cleanly. The **leak-under-JIT oracle** is the proof obligation for every slice — a mis-balanced
retain/release on the inlined push/pop shows up as non-zero residency immediately.

## Expected payoff, honestly

The helper round-trip is the bulk of the ~27 ns/call overhead, so S3+S4 could plausibly bring fib
from ~33 ns/call toward **~10 ns** — CPython/PHP-baseline territory, a **~2–3× on fib** and a broad
lift on all call-heavy code (Noeta's weakest axis). It will **not** reach LuaJIT's ~2 ns/call: that
needs trace-based cross-call inlining, a different (tracing-JIT) architecture, explicitly out of scope.

## Companion lever (separate, not part of this milestone)

Arbitrary-precision `int` marks every integer register "may-heap" (it can overflow-box), so fib's
arithmetic temps pay heap-aware refcount checks a fixed-width type would not. A **fixed-width `i64`**
(P-BITS Tier W) or a **JIT overflow range-proof** (prove a loop/recursion's ints stay within the
48-bit immediate range → stores go bare) removes that tax. Orthogonal to frame inlining; sequence
either after P-CALL S3 shows the frame win, or fold the range-proof in if it's cheap. Tracked with
P-BITS.
