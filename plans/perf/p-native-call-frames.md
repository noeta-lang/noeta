# P-CALL — native call frames (inline the call/return sequence into JIT code)

Status: **S1–S2 done; S3–S4 DROPPED (measured low ceiling). The redirect then landed the real win —
`bare-store arithmetic in call-bearing protos` (+6% fib), NOT a new int type.** Cumulative over the
whole fib arc (A+B CallGlobal + S2 + bare-store): **fib +12.2%**, loop +2.9%, assoc +4.2%, wordcount
−3.3% (a map-heavy codegen-layout artifact). Out-of-oracle (JIT is real-host only), gated by
`--jit-differential` (byte-identical `RunResult` + leak-under-JIT residency 0).

## Redirect outcomes — pursuing the three "reduce the work" levers

- **#3 fixed-`int` heap-aware tax → DONE via a JIT analysis, not a new type (`779b6c5`, +6% fib / +2.3%
  loop).** The tax wasn't (only) arbitrary-precision `int` — it was that a call-bearing proto is
  `heap_aware` and the bare-store analysis *failed closed* on the unmodeled `Call`/`CallGlobal`, so
  every store (incl. immediate arithmetic temps) paid the release check. Fix: (a) model `Call`/
  `CallGlobal` in `reg_effect` as `Def { dst, heap: true }` (sound — a call only redefines its result;
  the interp's post-bail re-entry at pc+1 matches), and (b) treat `Add/Sub/Mul/Div/Rem` results as
  immediate (sound — the native `Binary` bails to the interpreter *before storing* on 48-bit overflow,
  float is NaN-boxed). Oracle-proven: an unsound bare store = a skipped release = a leak, and
  leak-under-JIT residency stays 0. **This is the real fib lever, and it landed without a new int type.**
- **#1 shrink `Frame` (`upvalues: Vec<Value>` → `Box<[Value]>`) → WASH, reverted.** Built and verified
  (all oracles green) but measured +0.2% on fib — no signal. Shrinking `Frame` 24→16 B doesn't move the
  needle (confirms S2: the frame push isn't the bottleneck), and it adds `into_boxed_slice`/`Box::from`
  copies in the closure path. Not kept.
- **#2 cheaper register-window init → DEFERRED (not a bolt-on).** Skipping `reserve_window`'s zero-init
  needs per-call liveness (else `do_return`'s release loop reads garbage → crash); the frames-`Vec`
  reallocs are startup-only (recursion depth is bounded); skipping the return release loop needs a
  frame that is all-immediate, which fib is not (`n` + call results are may-heap). It requires a
  calling-convention change or frame-immediate tracking — its own medium/large track.

## Findings (S1–S2) — the measurement that redirected the milestone

- **S1 (`2ac6d4f`, done):** locked the `Frame`/`Vec`-header layout (`noeta_jit::FrameLayout`,
  `noeta_vm::frame_layout()` + probe + lock test). Behaviour-neutral foundation.
- **S2 (`775f96f`, done — the measurement-first slice):** inlined the direct-call hot return path
  (`OUTCOME_RETURNED` → continue, skipping the `jit_after_call` helper). Rigorous pinned/interleaved
  A/B (fib(35), n=25): **+2.3% min / +2.8% median**; loop control +0.0%. So **removing one bare
  hot-path helper call = ~2.4% ≈ ~0.7 ns/call.**
- **Conclusion:** fib is ~30 ns/call. Two helper calls per direct call, so removing *both* (full
  native inlining) recovers ~5% total. The remaining ~25 ns is the frame-setup **work** — reserve
  the register window (zero-init N slots), retain args, push/pop the `Frame` — which native inlining
  does **not** remove (it does the same work in machine code). **The per-call cost is the work, not
  the call overhead.** S3's native `Vec`-header writes (unsafe, realloc-hazard) would buy only the
  other ~2.4% → **not worth the risk.** DROPPED.
- **The real lever (redirect):** cut the per-call *work*, not the call overhead —
  1. **Shrink `Frame`.** `upvalues: Vec<Value>` (24 B, empty for every top-level fn) → `Option<Box<[Value]>>`
     (8 B) or a niche-packed form: smaller push, no empty-`Vec` write. Touches upvalue code; medium.
  2. **Cheaper register-window init.** `reserve_window` unit-initializes `num_registers` slots every
     call; a calling convention that passes args in the window and lets the callee init-before-read
     (GC-safe) would cut the zeroing. Larger.
  3. **The fixed-`int` heap-aware tax** (see the companion lever below) — likely the biggest single
     numeric-call win, and orthogonal to frames.
  These are separate tracks, each with its own measurement; none is "inline the sequence."

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

## Slices (as executed)

- **S1 — lock the layout (behaviour-neutral). DONE `2ac6d4f`.** `frame_layout()` + `FrameLayout` +
  `Vec`-header probe + lock test. jit-differential 431/0 unchanged.
- **S2 — inline the hot return path (measurement-first). DONE `775f96f`.** `OUTCOME_RETURNED` →
  continue in native, skip `jit_after_call` on the hot path. +2.4% fib. This slice's job was to
  *measure* whether the helper-call round-trip or the frame-setup work dominates — see Findings. It
  answered: the work dominates.
- **S3 — native Frame push/pop. DROPPED.** Would emit unsafe `Vec`-header writes (bump `len`, write
  the frame at `ptr + len*stride`) with a realloc-dangles-the-pointer hazard. The S2 measurement
  shows it buys ~2–3% for that risk (it removes the `jit_prepare_call` *call* but not its *work*).
  Not worth it. The S1 `FrameLayout` foundation stays in place should this ever be revisited.
- **S4 — native return transfer. DROPPED** (same reasoning — the return work is inherent).

## Refcount + soundness posture (non-negotiable)

Every native store stays refcount-exact via the existing heap-aware discipline (the shared register
stack makes the tier boundary free — P-VMT-FRAME's payoff). Bail-before-mutate holds: any guard
(capacity, upvalue-count, arity) decides *before* touching the stack so the interpreter re-runs the
op cleanly. The **leak-under-JIT oracle** is the proof obligation for every slice — a mis-balanced
retain/release on the inlined push/pop shows up as non-zero residency immediately.

## Expected payoff — REVISED by measurement

The original estimate (~2–3× on fib from inlining the frame sequence) was **wrong**, and the S2
measurement is why we found out cheaply. The helper *round-trip* is **not** the bulk of the per-call
cost — it is ~0.7 ns/call per helper (~2.4% each). The bulk is the frame-setup **work** (~25 of ~30
ns/call), which native inlining reproduces rather than removes. Realized: **S2's +2.4%, banked, safe.**
Ceiling of the whole "inline the sequence" idea: **~5%.** To actually move fib you must cut the work
(shrink `Frame`, cheaper window init, or the fixed-`int` tax) — separate tracks, above.

## Companion lever (separate, not part of this milestone)

Arbitrary-precision `int` marks every integer register "may-heap" (it can overflow-box), so fib's
arithmetic temps pay heap-aware refcount checks a fixed-width type would not. A **fixed-width `i64`**
(P-BITS Tier W) or a **JIT overflow range-proof** (prove a loop/recursion's ints stay within the
48-bit immediate range → stores go bare) removes that tax. Orthogonal to frame inlining; sequence
either after P-CALL S3 shows the frame win, or fold the range-proof in if it's cheap. Tracked with
P-BITS.
