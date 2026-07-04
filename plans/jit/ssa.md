# P-JSSA — SSA register promotion in the JIT (mem2reg)

**Status: PLANNED (sign-off pending).** The follow-on milestone to J0–J7: hold live VM registers in
Cranelift **SSA values** inside compiled code instead of round-tripping every operand through the
in-memory register stack.

## Why this is the next milestone (the measured case)

Every remaining scalar gap points at the same structural fact. The current codegen shape (J0) is one
Cranelift block per bytecode pc with **register state in memory**: each op loads its operands from
`regs[base + i]`, computes, and stores the result back. Cranelift never sees a value live across two
ops, so it cannot register-allocate, GVN, or hoist anything; every ALU op is fenced by loads and
stores the CPU must retire in order.

The evidence that this — and not any single op — is the ceiling:

- **J6 (native inline cache): correct, zero speedup, reverted.** Making the field read native didn't
  move a field-bearing loop because the loop was bound by memory-resident register traffic and
  dependent loads, both tier-independent.
- **J7 (bare stores): +19–28% on numeric loops** — from merely *eliding refcount checks* on those
  memory stores. The stores themselves remain.
- **P-CALL S2 (measurement-first): the ~30 ns/call cost of `fib` is the frame-setup *work*** —
  zero-initialize the register window, retain args, push/pop `Frame` — not helper-call overhead
  (removing a hot-path helper call bought 2.4%). A calling convention that passes values in machine
  registers and materializes frames lazily is the only thing that removes that work, and it requires
  values to *be* in registers first.
- **Cross-language standings (2026-07-04 xlang suite, compute ms):** loop 10M — Noeta-JIT 86.8 vs
  PHP-JIT 17.2 / LuaJIT 13.9; fib(32) — 188.7 vs 25.0 / 15.8. The engines ahead keep hot values in
  machine registers; we are 5–12× behind on exactly the workloads where that is the whole difference.

Interpreter-side, dispatch is ~3.3 ns/op — the floor for a `match`-based interpreter. There is no
cheaper lever left on either tier: the next win must change *where values live*.

## Goal & non-goals

**Goal.** Within a compiled prototype, VM registers that are live between ops become Cranelift SSA
values (block parameters across CFG edges). The in-memory register stack is touched only at **region
boundaries**: loads at entry (pc-0, OSR header, resume-after-call), spills at every bail/deopt edge,
and synchronization around ops that inspect the frame (runtime helpers, calls). Cranelift's register
allocator then does what it exists to do.

Targets (pinned, same harness as the xlang suite): **loop 10M ≤ ~30 ms** (from 86.8 — the PHP-JIT
class), **fib(32) ≤ ~60 ms** (from 188.7 — ahead of PHP-interp; parity with the JITs additionally
needs S4's calling convention). Treat these as direction, not promises; each slice re-measures.

**Non-goals.**
- Not a tracing JIT, not a new IR, no bytecode changes. The unit of compilation stays the prototype;
  the input stays the existing `Op` stream.
- No language-semantics or refcount-behaviour change. `--jit-differential` byte-identity and
  leak-residency-0 remain the definition of correct.
- The sandbox/differential path stays interpreter-only, byte-identical, Cranelift-free.
- Heap-*op*-dominated loops (BuildString / CallMethod per iteration) are out of scope — they bail per
  iteration regardless of how registers are held, and the `worth_osr`/`worth_compiling` gate already
  leaves them in the interpreter. Their lever is the string/allocation cluster, not this milestone.

## The refcount problem, and the v1 dodge

If a *heap* value lives in SSA and its memory slot goes stale, every spill point must reconstruct
exact ownership (which slot owns which reference, what tier-0 would have released where) — the
highest-risk bookkeeping in the whole design. But the workloads this milestone targets are numeric:
their hot registers are **provably immediate** exactly where J7's `heap_in_map` analysis already
proves it (a natively-stored arithmetic result is always immediate; params are guarded at entry;
overflow bails before the store).

So v1 promotes **only provably-immediate registers** — an immediate is a plain NaN-boxed u64 with no
ownership, so spilling one is a plain store and eliding its intermediate stores changes no refcount.
Heap-holding registers keep their memory residency and current (J7-refined) store discipline. This
captures the loop/fib win with zero new refcount surface; extending SSA residency to heap values is a
deliberately separate, last slice (S5) that can be dropped if its measured value doesn't justify it.

## Staged slices

| # | Slice | Delivers | Risk |
|---|---|---|---|
| **S0** | **Region liveness + value-location maps.** Per-pc live-in/live-out over the tier-0 CFG (reuse `reachable_pcs`/`reg_effect` machinery), classified immediate-vs-may-heap by `heap_in_map`. A `ValueLoc` table per pc: each register is `InSlot` or `Ssa(var)`. No codegen change — landed as analysis + unit tests locking the contract (the J7 pattern). | Analysis only; fails closed to `InSlot`. |
| **S1** | **Straight-line promotion.** Within each basic block, an immediate register written then read stays an SSA value; slot stores happen only at block exit and bail edges (spill the block's dirty SSA values, then return the resume pc — bail-before-mutate becomes *spill-then-bail*, same interpreter contract). First measurable win: intra-block load/store traffic gone. | Spill-map correctness at bail edges — oracle-gated. |
| **S2** | **Cross-block promotion (the payoff).** Live immediate registers cross CFG edges as **block parameters**; the loop-carried accumulator/counter never touches memory inside the loop. Entry points (pc 0, OSR headers, resume-after-call) load their live-in set from slots once. `LoadConst`/LICM/GVN become Cranelift's problem. Expect the bulk of the loop-benchmark win here. | Block-param plumbing across the per-pc block layout; OSR entry state. |
| **S3** | **Skip the register-window zero-init** (P-CALL deferred lever #2). With definite-assignment from S0, a frame whose registers are all written-before-read (or covered by entry spills) doesn't need `reserve_window`'s zero-fill; `do_return`'s release loop instead walks a recorded live-set (or the frame spills unit to dead slots at deopt only). Removes a per-call `memset`-shaped cost. | Frame-teardown contract; leak oracle is the judge. |
| **S4** | **SSA calling convention for native→native direct calls.** A direct call passes args as SSA values (Cranelift call args) and receives the return value in a register; the callee's frame is materialized (slots + `Frame` push) **only on its deopt path**. The caller's frame-setup work (zero-init + retain-immediates + push/pop) disappears on the all-native path — the actual `fib` lever P-CALL identified. Fallback: any non-direct-able call keeps today's `jit_prepare_call` protocol. | The deopt path must reconstruct a frame mid-call ("frame reconstruction") — the subtlest slice; do after S1–S3 are proven. |
| **S5** | **(Optional, measure-first) heap values in SSA.** Ownership-aware promotion of may-heap registers (spill = store + the release tier-0 would have done at the overwrite). Only if a workload demonstrates the win after S1–S4; J6's lesson says don't assume. | Refcount bookkeeping — highest risk, lowest proven value. Drop by default. |

Each slice lands only with: `--jit-differential` 0-divergence / 0-leak at full corpus coverage,
standard differential + leak oracle untouched (sandbox is tier-0 by construction), criterion
before/after on `vm_jit/*` (`loop`, `float`, `osr`, `global`, `fib`, `mixed`, `widefield` — the last
two must not regress), and the pinned xlang loop/fib numbers recorded in this doc.

## Design notes (constraints discovered by J0–J7 that S1+ must honour)

- **Deopt contract.** Today: a native op decides its bail *before mutating any state*, returns the
  resume pc, and tier-0 re-runs the op from a clean state. With SSA the equivalent is: **all spills a
  bail edge needs are emitted on that edge**, before the return — the interpreter must find every
  register slot holding exactly what tier-0 would have there at that pc. S0's per-pc `ValueLoc` table
  is the single source of truth; a lock test enumerates bail edges and asserts spill completeness.
- **Entry-pc dispatch.** Resume-native and OSR re-enter mid-frame (J3/J5). Every extra entry point is
  a region boundary: its live-in registers load from slots. The existing entry-pc compare chain stays;
  each entry block gains its load sequence.
- **Helper calls are sync points.** A runtime helper (`jit_call`, `jit_run_leaf_op`, …) reads and
  writes the frame's slots. Before the call: spill the helper's may-observed set (conservatively, all
  dirty SSA); after: reload what stays live. A leaf-op-heavy proto therefore sees little benefit —
  expected, out of scope (see non-goals).
- **`Value::NANBOX` stays the single source of truth** for tag checks; SSA promotion changes where a
  value lives, never its encoding.
- **Fail closed, per proto.** Any op the analysis can't model ⇒ that proto compiles in today's
  all-slots mode (the J7 posture). Coverage grows by modeling, never by assuming.
- **`worth_osr`/`worth_compiling` gates unchanged.** A loop that bails per iteration stays
  interpreted; SSA doesn't change *which* protos compile, only how well the compiled ones run.
- **Compile time.** Block params × per-pc blocks grow Cranelift's work. Hot-counter promotion (50)
  bounds exposure; measure compile latency in the criterion pass, and if needed merge straight-line
  pc-blocks into real basic blocks first (an S1 refactor that also simplifies S2).

## Honest ceiling

This lands us in the **method-JIT class** (PHP-JIT, ~0.9–1.7 ns/op on these loops), not LuaJIT's
trace-specialized ~0.3 ns/op. After S4, remaining fib distance is call-protocol fundamentals (LuaJIT
inlines and trace-specializes the recursion). That is a different architecture, not a slice — if
scalar parity beyond PHP-JIT ever matters, it's a new sign-off. Meanwhile the design's own strengths
(SoA columns, strcat, startup) already lead the field and stay untouched.

## Relationship to other tracks

- **Depends on:** nothing new. Builds directly on P-VMT-FRAME (contiguous register stack = deopt
  state), J3 (direct calls, resume-native), J5 (OSR entries), J7 (`heap_in_map` + its lock tests).
- **Feeds:** the string/allocation cluster (fused `get_or` landed separately; interning + intrusive
  GC registry are orthogonal and can interleave between slices).
- **Supersedes:** the dropped P-CALL S3/S4 (native frame-Vec writes) — S4 here removes the frame work
  instead of inlining it, which is why it can win where P-CALL measured ~5%.
