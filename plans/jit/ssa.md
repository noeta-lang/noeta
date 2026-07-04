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
| **S0** | ✅ **DONE.** **Region liveness + value-location maps** (`noeta-jit/src/plan.rs`). Per-pc backward liveness over sound all-op successors (`succ_all` covers the `Match*`/`Coalesce` edges the arithmetic-whitelist `analysis_succ` never sees; unmodeled op = reads-all, fail-closed **per op**) + `ssa_ok` = the complement of the J7 bare-store map. 6 contract-locking tests. | Analysis only; fails closed to `InSlot`. |
| **S1+S2** | ✅ **DONE (one slice — see "What landed" below).** Promotion via **`cranelift-frontend` `Variable`s** (`declare_var`/`def_var`/`use_var`), which build the block parameters at merges automatically — straight-line *and* cross-block (loop headers included) in one mechanism, which is why the two planned slices collapsed into one. Plus **known-constant register inlining** (`plan::const_reg_bits`) and `opt_level=speed`. fnloop **1.36×**, toploop **1.18×**, float loop **1.23×**; `--jit-differential` 0-div/0-leak first run. | Spill-map correctness at bail edges — oracle-gated. |
| **S3** | **Skip the register-window zero-init** (P-CALL deferred lever #2). With definite-assignment from S0, a frame whose registers are all written-before-read (or covered by entry spills) doesn't need `reserve_window`'s zero-fill; `do_return`'s release loop instead walks a recorded live-set (or the frame spills unit to dead slots at deopt only). Removes a per-call `memset`-shaped cost. | Frame-teardown contract; leak oracle is the judge. |
| **S4** | **SSA calling convention for native→native direct calls.** A direct call passes args as SSA values (Cranelift call args) and receives the return value in a register; the callee's frame is materialized (slots + `Frame` push) **only on its deopt path**. The caller's frame-setup work (zero-init + retain-immediates + push/pop) disappears on the all-native path — the actual `fib` lever P-CALL identified. Fallback: any non-direct-able call keeps today's `jit_prepare_call` protocol. | The deopt path must reconstruct a frame mid-call ("frame reconstruction") — the subtlest slice; do after S1–S3 are proven. |
| **S5** | **(Optional, measure-first) heap values in SSA.** Ownership-aware promotion of may-heap registers (spill = store + the release tier-0 would have done at the overwrite). Only if a workload demonstrates the win after S1–S4; J6's lesson says don't assume. | Refcount bookkeeping — highest risk, lowest proven value. Drop by default. |

Each slice lands only with: `--jit-differential` 0-divergence / 0-leak at full corpus coverage,
standard differential + leak oracle untouched (sandbox is tier-0 by construction), criterion
before/after on `vm_jit/*` (`loop`, `float`, `osr`, `global`, `fib`, `mixed`, `widefield` — the last
two must not regress), and the pinned xlang loop/fib numbers recorded in this doc.

## S1+S2 — what landed (Variables, constant inlining, and the boxing finding)

**Mechanism.** One `cranelift-frontend` `Variable` (i64) per VM register. Reads (`read_reg`) use
the variable exactly where the plan proves the register immediate (`ssa_ok`); writes (`store_reg`)
are a pure `def_var` where both the old and new occupant are provably immediate, a write-through +
`def_var` at a heap→immediate transition (the released pointer must not linger in the slot), and
the plain J7-refined slot store elsewhere. Every native entry point (guarded pc-0, resume,
OSR header) passes through an init block that `def_var`s all variables from the slots, so every
variable is defined on every path. Bail edges spill resident ∩ live (`spill_ssa`) before returning
the resume pc — *spill-then-bail*, same interpreter contract; helper ops (`jit_call`,
`jit_run_leaf_op`) spill before and reload after (sound because pre-spill made live slots current,
a helper-written slot is fresh, and a dead register's stale value is never read before its next
def). `seal_all_blocks` at the end resolves all block params. A prototype whose heap analysis
failed closed (leaf-op/field code) gets **no** variables — byte-identical codegen, no regression.

**Two things the first measurement forced:**
- **`opt_level=speed`.** At Cranelift's default `none`, block-param code was **2× slower** than the
  memory form (fnloop 54→79 ms) — the SSA form needs the mid-end. With `speed` it flipped to a win.
  Cost: ~1–3 ms compile time per hot proto (visible as ~2% on the leaf-op `forrange`, whose codegen
  is otherwise unchanged).
- **Known-constant register inlining** (`plan::const_reg_bits`). Promoting LICM's hoisted-constant
  registers made the globals loop **5% slower**: with ~7 live ABI pointers plus the retain/release
  helper calls on `LoadGlobal`/`StoreGlobal`'s cold paths, regalloc pushed the promoted constants
  to the machine stack — same traffic as the frame slots, plus shuffling (read straight off the
  `NOETA_JIT_DISASM=1` dump, a debug tool added in this slice). A register written exactly once by
  a `LoadConst` of an immediate, with no read reachable from pc 0 that bypasses the def, is now
  read as an **inlined `iconst`** instead: no variable, no block param, no pressure — and the
  egraph folds its tag checks and unboxing statically. That flipped toploop to +18% and widened
  fnloop's win.

**Measured (pinned, interleaved min-of-9, end-to-end `noeta run`):** fn-local int loop 10M
53.7→39.4 ms (**1.36×**), top-level globals loop 10M 70.7→59.9 ms (**1.18×**), float loop
36.6→29.8 ms (**1.23×**), fib(32) ~1.02× (its lever is S4), strcat/assoc/struct-field loops
neutral (1.00–1.01×). Gates: `--jit-differential` 432/0-divergence/0-leak (890/891 native),
standard differential 432/0, conformance 443, leak 0, workspace green, clippy+fmt clean.

**The finding that reshapes the next slice: with loads/stores gone, the NaN-box chain is the
floor.** The promoted loop still pays, per op: two `is_small_int` tag checks, two unbox
shift-pairs, the 48-bit overflow fit-check, and a re-box — because a `Variable` holds the *boxed*
word, and the egraph cannot fold box/unbox chains through loop-header block params (they are
opaque φs). LuaJIT/PHP-JIT keep hot values **unboxed with checks hoisted out of the loop**. So the
next slice is **typed promotion (T1)**: a forward numeric-kind dataflow in the plan (lattice
`{Bot, Int, Bool, Imm}` describing the *native-path* state — a natively-completed arith `Binary`
with statically-int operands is Int, a comparison is Bool, merges meet), plus a second **raw**
variable per register holding the unboxed value. Def sites with a statically-known kind define
both forms (box once); reads at statically-Int pcs use the raw form and skip the tag checks and
unboxing entirely; `CondBranch` on a statically-Bool register branches on the raw bit (no
false/true/bail compare chain); spills always store the boxed form. Kind analysis is per-op
fail-closed like liveness. Expected: the remaining ~2–3× on fnloop-class loops.

The globals loop's ceiling after T1 is the globals array itself (every iteration re-loads/stores
`i`/`total` slots with unbound/heap guards) — its lever is register-allocating top-level locals
(the deferred deeper-F1, a compiler track, out of P-JSSA's scope).

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
