# JIT milestone (P-JIT) — closing the scalar/loop/call gap to a JIT engine

**Status: planning (proposal for sign-off).** The VM-throughput arc (P-VMT: S0–S5 + RMW, GSLOT, CBR,
LICM) took the interpreter as far as *interpreter-level* wins go — dispatch is now ~3.3 ns/op, near the
floor for a `match`-based switch interpreter. The remaining ~10–35× gap to PHP 8.4 on hot scalar / loop
/ call code (`loop 10M` ~35×, `fib(32)` ~17×) is structural: PHP's **tracing JIT** runs the equivalent
of ~0.3 ns/op. No interpreter tweak crosses that. This milestone adds a **JIT** — the only thing that
does.

## Goal & non-goals

**Goal.** Native-compile hot functions so the fast path (integer/float arithmetic, comparisons,
branches, register/local/global access, direct calls) runs as machine code instead of dispatched
bytecode, calling back into the existing runtime for everything else. Target: **within ~2–4× of
PHP-JIT** on the loop/call benchmarks, from ~17–35× today.

**Non-goals (v1).**
- Not a tracing JIT. A **method JIT** (compile whole functions) is simpler and enough for the first
  large win; tracing/OSR is a later slice.
- Not compiling *everything*. Ops outside the supported set keep running in the interpreter (per
  function, then per region). The supported set grows slice by slice.
- No change to language semantics, the type system, or the object model. The JIT is a pure execution
  accelerator behind the same `RunResult`.
- Not the sandbox/differential path. The deterministic sandbox the conformance differential runs stays
  **interpreter-only** — the JIT is a real-host accelerator (like the real-thread isolates), so the
  eval↔interpreter differential is untouched. The JIT gets its **own** oracle (see Validation).

## Why this is worth a milestone (and the honest caveat)

The design already *wins* where it targets — SoA column-vector math beats PHP-JIT (1.7–4.4×). The JIT
is specifically about **general scalar/loop/call parity** with a 25-year-tuned engine. That is a real,
weeks-scale effort with a large surface (codegen, a runtime-call ABI, refcount correctness, a new
oracle, tiering). It is staged below so it delivers incremental value — **J1 alone** (integer fast
path) already accelerates the most common hot loops. Sign-off should confirm the goal is worth the
cost versus spending the effort on the design's strengths.

## Backend decision — Cranelift

| Option | Verdict |
|---|---|
| **Cranelift** (`cranelift-jit`, `cranelift-frontend`) | **Chosen.** Pure-Rust codegen backend (Wasmtime's), built *for* JITs: fast compile, position-independent code, a clean SSA builder, no C++/LLVM toolchain. Good-enough codegen (not peak, but far past an interpreter). Composes with a Rust runtime-call ABI. |
| LLVM (`inkwell`) | Best codegen, but a heavy native dependency, slow compile, C++ ABI surface — overkill for a baseline JIT and a poor fit for a from-scratch Rust project. Revisit only if Cranelift's codegen becomes the bottleneck. |
| Hand-rolled asm / `dynasm` | Max control, max effort, per-arch. No. |
| Copy-and-patch templates | Fast compile, decent code, but exotic tooling and its own correctness surface. Interesting for a *later* tier-1-fast variant; not v1. |

New crate **`lang-jit`** (depends on `lang-bytecode`, `lang-value`, `lang-object`, and a thin runtime
seam). Behind a **`jit` cargo feature**, off by default — so the default build, the sandbox, and the
conformance harness are byte-identical and dependency-free, exactly like `real_isolates` gates the
real-thread path today.

## Architecture

**Tiering.** Tier 0 = the interpreter (unchanged). Tier 1 = JIT. A per-proto call/back-edge **counter**
promotes a proto to tier 1 once hot; until then it interprets. `run_module_with_host` on the real path
consults a compiled-code cache keyed by proto index.

**What the JIT compiles (the ABI).** A compiled function has the signature
`fn(vm: *mut Vm, regs: *mut Value, base: usize) -> ControlOutcome`. It operates directly on the
contiguous register stack (P-VMT-FRAME) at `regs[base + i]`. For any op it does not implement inline,
it **calls a runtime helper** (a plain `extern "C"`-ish Rust fn) — `jit_retain(v)`, `jit_release(vm,
v)`, `jit_call(vm, callee, ...)`, `jit_make_list(...)`, `jit_method(...)`, etc. — reusing the exact
interpreter logic so behaviour is identical by construction. The fast path (int/float ALU, compares,
branches, `LoadConst`, `Move`, slot-indexed global/local access) is emitted as native Cranelift IR;
NaN-box tag checks are inlined, with a **guard-and-bail** to the interpreter when a value isn't the
expected primitive shape (the deopt seam).

**Fallback / deopt.** v1: a proto is JIT-eligible only if every op is in the supported set; otherwise
it stays tier 0 forever (recorded, `log`-visible). As the set grows, move to **per-op bail**: an
unsupported or guard-failing op transfers control back to the interpreter at that `pc` (the register
stack is shared, so state is already consistent — the key payoff of the contiguous register design).

**Refcount correctness.** The differential + leak oracle demand byte-exact `retain`/`release`. The fast
path (primitive ALU) touches no refcounts. Every heap-value move/store/drop the JIT emits calls the
same `retain`/`release_value` the interpreter uses — no reimplementation. This is the highest-risk area
and gets the most oracle coverage.

**Isolates/threads.** Cranelift emits position-independent code; a compiled proto is shared read-only
across isolate threads (like `Arc<Module>`). Each isolate keeps its own tier-0/tier-1 decision and
counters. The JIT never runs on the deterministic sandbox path.

## Validation — the JIT's own oracle (non-negotiable)

The conformance differential (eval ↔ interpreter, deterministic sandbox) does **not** exercise the JIT.
So the JIT ships with its own gate, mirroring the existing discipline:

1. **`--jit-differential`** (new `lang-conformance` mode): run every corpus program through the
   interpreter and through the JIT (real host, forced tier-1 where eligible) and assert **byte-identical
   `RunResult`** (stdout, diagnostics, exit). Same "0 skipped / backends agree" bar. Eligible-only at
   first; the eligible set grows per slice, and the gate reports coverage (`N/total protos JIT-run`).
2. **Leak oracle under JIT** — the residency-0 check with tier-1 forced on, proving refcount exactness.
3. **`force_jit` test flag** — compile-and-run every eligible proto even when cold, so tests hit tier 1
   without needing to be hot.
4. **Criterion before/after** on `vm_dispatch/*`, `vm_recursion/fib`, `vm_map_rmw` with the `jit` feature
   — the perf claim per slice.

A JIT slice does not land until `--jit-differential` is 0-divergence and the leak oracle is residency-0
on every eligible program.

## Staged slices

| # | Slice | Delivers | Risk |
|---|---|---|---|
| **J0** | **Foundation** — `lang-jit` crate + `jit` feature, Cranelift wired, the compiled-code cache + tier-0/1 dispatch seam, the runtime-helper ABI skeleton, hot-counter promotion, and the `--jit-differential` + leak-under-JIT oracle harness. No op compiled yet (every proto bails to tier 0) — proves the *plumbing* and the oracle. | Setup; the ABI/deopt seam is the crux to get right early. |
| **J1** | **Integer fast path** — compile protos whose ops are all in {`LoadConst`(prim), `Move`, `Binary`(int arith/cmp), `WideInt`, `CondBranch`, `Jump`, `JumpIf*`, `LoadGlobal`/`StoreGlobal`/`TakeGlobal` slot, `Return`, `Halt`}. Guard-and-bail on non-int operands. First real native code — accelerates integer loops (`loop 10M`, the ablation loops). | Codegen correctness; NaN-box guards; the first oracle run. |
| **J2** | **Floats + control** — float ALU, `f32`, tuples make/index, `Narrow`/`IsType`, more branch shapes. Widens the eligible set. | Incremental. |
| **J3** | **Calls** — inline the direct-call fast path (known callee proto, arity match) with a native call into another compiled proto (or a helper trampoline to tier 0); `fib`-class recursion. | Call ABI, stack/frame interaction, recursion depth. |
| **J4** | **Heap & collections** — retain/release inlined, allocation + list/map/set/string/field ops via runtime helpers keeping refcount exact. Most real programs become fully JIT-eligible. | **Refcount exactness** — the leak oracle is the gate. |
| **J5** | **Tiering polish + OSR** — on-stack replacement so a hot loop enters tier 1 mid-execution (not only at call boundary); counter tuning; per-op bail replacing whole-proto eligibility; compile-time budget. | Deopt/OSR is subtle; do last. |

Each slice: `--jit-differential` 0-divergence, leak residency 0 under forced JIT, a criterion
before/after, and `git` commit per green slice (standing directive).

## Open questions (resolve at sign-off)

- **Method JIT vs tracing.** Proposal: method JIT first (J1–J4), OSR/tracing deferred to J5+. Tracing
  would better handle megamorphic/branchy code but is a much larger harness.
- **Compile trigger & budget.** A simple call+back-edge counter with a fixed threshold; compilation is
  synchronous on the calling thread in v1 (background compilation is a later refinement).
- **Cranelift as a hard dep even when gated.** The `jit` feature pulls `cranelift-*`; the default build
  must stay clean. Confirm the feature-gating keeps `cargo build`/sandbox/conformance dependency-free.
- **Debug/observability.** A `--dump-jit` (Cranelift IR / disassembly) analogous to `lang dump`, for the
  same agentic-debugging reasons.
- **Is the goal worth it?** The strategic call (§"Why this is worth a milestone"): general scalar-loop
  parity with a JIT engine vs investing in the design's proven strengths. Sign-off decides.
