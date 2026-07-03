# JIT milestone (P-JIT) — closing the scalar/loop/call gap to a JIT engine

**Status: J0 + J1 + J2 + native globals DONE (integer/float/global fast paths + per-op bail, ~4–7× on native loops); J3+ pending.** The VM-throughput arc (P-VMT: S0–S5 + RMW, GSLOT, CBR,
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
| **J0** | ✅ **DONE.** **Foundation** — `lang-jit` crate + `jit` feature, Cranelift wired, the compiled-code cache + tier-0/1 dispatch seam, the runtime-helper ABI skeleton, hot-counter promotion, and the `--jit-differential` + leak-under-JIT oracle harness. No op compiled yet (every proto bails to tier 0) — proves the *plumbing* and the oracle. | Setup; the ABI/deopt seam is the crux to get right early. |
| **J1** | ✅ **DONE.** **Integer fast path** — compile protos whose ops are all in {`LoadConst`(imm), `Move`, `Drop`, `Binary`(int `+ - * / %` and `== != < <= > >=`), `CondBranch`, `Jump`, `JumpIf*`, `Return`/`Halt` as bail points}. Guard-and-bail on non-int operands, zero divisor, and 48-bit overflow. First real native code — **~6–7.5× on register-local integer loops**. (Globals + `WideInt`/bitwise deferred; see below.) | Codegen correctness; NaN-box guards; the first oracle run. |
| **J2** | ✅ **DONE (floats).** **Float fast path** — native f64 ALU (`+ - * /`) and ordered comparison (`== != < <= > >=`), dispatched from the same `Binary` by a runtime int-vs-float type check; NaN results canonicalized to match `Value::float` bit-for-bit. **~6.5× on float loops.** (f32, tuples, `Narrow`/`IsType` deferred — see below.) | Incremental. |
| **J3** | **Calls** — inline the direct-call fast path (known callee proto, arity match) with a native call into another compiled proto (or a helper trampoline to tier 0); `fib`-class recursion. | Call ABI, stack/frame interaction, recursion depth. |
| **J4** | **Heap & collections** — retain/release inlined, allocation + list/map/set/string/field ops via runtime helpers keeping refcount exact. Most real programs become fully JIT-eligible. | **Refcount exactness** — the leak oracle is the gate. |
| **J5** | **Tiering polish + OSR** — on-stack replacement so a hot loop enters tier 1 mid-execution (not only at call boundary); counter tuning; per-op bail replacing whole-proto eligibility; compile-time budget. | Deopt/OSR is subtle; do last. |

Each slice: `--jit-differential` 0-divergence, leak residency 0 under forced JIT, a criterion
before/after, and `git` commit per green slice (standing directive).

## J0 — what landed (foundation)

New crate **`lang-jit`** behind `lang-vm`'s `jit` feature (default build pulls **0** Cranelift crates;
`--features jit` pulls the 29-crate Cranelift stack — confirmed via `cargo tree`). Pieces:

- **Cranelift wired end to end.** `Jit::new(helpers)` builds a `JITModule` on the host ISA (via
  `cranelift-native`), registering the runtime-helper symbols. `Jit::compile(module, proto)` emits,
  finalizes, and caches native code for a prototype, keyed by prototype index (`Vec<Option<CompiledFn>>`).
- **The tier-1 ABI.** `CompiledFn = unsafe extern "C" fn(vm: *mut c_void, regs: *mut Value, base: usize) -> u8`,
  operating directly on the shared register stack (`regs[base + i]`, P-VMT-FRAME). The `u8` is an
  `Outcome` (`Bail` = interpret this frame in tier 0; `Returned` = compiled code did the return
  protocol, reserved for J1+).
- **The J0 body is a *bail stub*.** For every prototype, the emitted code calls one runtime helper
  (`lang_jit_observe`, proving the helper ABI links and the VM pointer round-trips) and returns `Bail`.
  So tier 1 runs, then hands the frame straight back to the interpreter — byte-identical output, by
  construction.
- **Dispatch seam** at the top of the interpreter's `'reload` window (lang-vm), fired only at fresh
  frame entry (`pc == 0`): consult the compiled cache, call the native entry, act on its `Outcome`.
  One seam covers every call shape (top-level, `Call`, `CallMethod`, method dispatch) because they all
  re-enter `'reload`.
- **Hot-counter promotion.** Per-proto entry counter; a prototype compiles once it crosses
  `JIT_HOT_THRESHOLD` (50), or immediately under `force_jit`. Workers/isolates never get a JIT
  (`JITModule` is `!Send`); the deterministic sandbox stays tier 0.
- **The JIT's own oracle.** `cargo run -p lang-conformance --features jit -- --jit-differential`: runs
  every corpus program on the interpreter *and* the forced tier-1 JIT (same `SandboxHost`) and asserts
  byte-identical `RunResult` **and** zero heap residency under JIT. Result: **419 matched, 0 skipped,
  813 prototypes JIT-compiled, 0 divergence, 0 leaks.** Gated corpus test `jit_differential_tiers_agree`
  + a lang-vm unit test proving the native stubs actually execute (via the observe counter).
- **Unsafe discipline.** `lang-vm` downgrades `unsafe_code` from the workspace `forbid` to `deny` so the
  single native-call site opts in with an explicit `#[allow(unsafe_code)]`; everything else stays
  unsafe-free. `lang-jit` re-states the lint table minus the forbid (like `lang-value`).

No criterion before/after for J0 — every prototype bails, so there is **no perf delta yet**; the
benches begin at J1 (integer fast path), the first slice that emits real native arithmetic.

## J1 — what landed (integer fast path)

The first slice that emits real machine code. `Jit::compile` now checks each prototype against the J1
op set and emits either a **native integer body** or the J0 bail stub; the interpreter dispatches
through both at frame entry.

- **The deopt contract, simplified.** The tier-1 ABI now returns a **`u32` resume pc** (not an
  outcome). A compiled body runs the ops it knows and, at the first it doesn't (`Return`, `Halt`, or a
  failed guard), returns that op's `pc`; the interpreter continues from there. The shared register
  stack makes this free — the native code did the exact register writes the interpreter would have, so
  the window is already consistent at the handoff. `0` = "interpret the whole frame" (the bail stub).
- **The immediate invariant → zero refcounts.** The body first **guards every parameter is an
  immediate** (bails to pc 0 if any is a heap pointer). Locals start `unit`, so from there *every
  register holds an immediate for the entire native run* — which means every `retain`/`release` the
  interpreter would do is a no-op, so the fast path emits **none**. This is what makes native integer
  code both correct (no missed refcount) and fast.
- **Per-op guards, bail-before-write.** A `Binary` bails if either operand is not a small int, if a
  `/`/`%` divisor is zero (interpreter raises E0008), or if the arithmetic result overflows the 48-bit
  immediate range (a big int must heap-box — the interpreter does it). A `CondBranch` bails on a
  non-bool (E0007). Every bail happens before any store, so re-execution is clean. `JumpIfTrue/False`
  need no guard (a non-bool is simply "not taken", matching `as_bool()`).
- **Codegen shape.** One Cranelift block per bytecode pc; register state lives in memory (the `regs`
  array), so blocks carry no SSA params — only the frame base pointer (computed once in the entry
  block) crosses in. Dead-code pcs get a trivial bail so they never reference that pointer from a
  non-dominated block. Value encoding comes from **`lang_value::Value::NANBOX`** — one source of
  truth, `lang-value`-tested against `Value::int`/`bool`/`unit`, so the inlined tag/box/unbox math
  can't drift from the interpreter.
- **Result.** `--jit-differential`: **419 matched, 0 skipped, 69/813 prototypes native, 0 divergence,
  0 leaks.** Bench `vm_jit` (interp vs forced JIT on a register-local integer `while` loop): **100k
  4.88 ms → 0.80 ms (~6.1×), 1M 43.2 ms → 5.80 ms (~7.5×).** lang-vm tests add a native-while-loop
  correctness test and an overflow-bail test.
- **Deferred to later slices:** globals (`LoadGlobal`/`StoreGlobal`/`TakeGlobal` — they touch heap
  values, so they need the runtime-helper retain/release path, folded into J4), the fixed-width
  `WideInt` and bitwise/shift ops, and `for`-range loops (they lower to `MakeRange`/`IterSnapshot`,
  outside the op set — a `while` loop is the J1-eligible shape).

## J2 — what landed (float fast path)

Floats are immediates (NaN-boxed, no heap), so they extend J1's zero-refcount invariant with no new
machinery — the natural next slice. A numeric `Binary` now runtime-dispatches on its operands
(the bytecode is untyped): **both small ints → the J1 integer path; both f64 floats → the new float
path; anything else** (mixed int/float, f32, objects) **→ bail** to the interpreter, which does the
widening/coercion. Eligibility is unchanged — a float `Binary` was already eligible under J1, it just
bailed every time; J2 makes it compute natively.

- **Native f64.** `fadd`/`fsub`/`fmul`/`fdiv` and `fcmp` with the interpreter's exact predicates:
  ordered comparisons (false on NaN) for `< <= > >= ==`, unordered `!=` (true on NaN) — matching
  `partial_cmp`→`None`→false and `!(a==b)`. A NaN arithmetic result is **canonicalized to the standard
  quiet NaN**, bit-for-bit `Value::float`, so it can never collide with the tag space (and matches the
  interpreter exactly). `is_float` is `(bits & qnan) != qnan`; the f64 is read/written by `bitcast`.
- **Bails:** float `%` (`fmod` is a libcall, not an instruction — rare), and any mixed/f32 operand
  pairing. All correct, just interpreted.
- **Result.** `--jit-differential` still **419 matched / 0 skipped / 0 divergence / 0 leaks** (69
  native prototypes — the float path changed *what those prototypes do*, not which are eligible). Bench
  `vm_jit/float_*` (interp vs forced JIT, register-local f64 `while` loop): **100k 3.83 ms → 0.59 ms
  (~6.5×)**. lang-vm adds a native float-loop test and a division/NaN/ordered-compare test.
- **Deferred:** `f32` scalar arithmetic (rare — f32 is mostly packed-list/SIMD territory, which is
  heap), tuples make/index and `Narrow`/`IsType` (heap values / type machinery → land with the heap
  slice J4, not here).

## Native globals + per-op bail — what landed (unlocks top-level loops)

The b_loop-class **top-level scripting loop** (a `while` over global `mut` accumulators, then `echo`)
was the target. Two changes got it native:

- **Prereq — `Vm.globals: Vec<Option<Value>>` → `Vec<Value>` + an `unbound` sentinel** (a separate
  behavior-neutral commit). `Option<Value>` is 16 bytes with an unstable layout the JIT can't inline;
  a plain `Vec<Value>` slot is one 8-byte word with a sound, stable layout — and half the size, with a
  compare-not-match unbound check. The globals array never grows, so its base pointer is stable for the
  whole run; it's passed as a **4th ABI argument** (`globals: *mut Value`).
- **Per-op bail replaces whole-proto eligibility.** Eligibility is now "the prototype has *at least
  one* compilable op"; inside, any op the JIT can't compile bails at its pc (the `_ => return pc` was
  already there). So a top-level prototype full of `Call`/`Echo`/`Stringify` still compiles its hot
  loop and bails at the first thing it can't — instead of being rejected wholesale. Coverage jumped
  **69 → 722 of 813 prototypes native**.
- **Native `LoadGlobal`/`StoreGlobal`/`TakeGlobal`**, guard-and-bail like the rest: a `LoadGlobal`
  bails if the slot is unbound (E0005) or holds a heap value (needs `retain`); an immediate copies with
  no refcount (its `retain` is a no-op) — preserving the immediate invariant. `StoreGlobal` consumes an
  immediate source: on the **first bind** (old slot unbound) it writes the slot and calls a tiny
  `note_global_bound` helper to record `global_order` for teardown (a `Vec` push can't be inlined); on
  reassign it bails on a heap old value (its `release` may run a destructor) and otherwise overwrites in
  place. `TakeGlobal` bails on unbound/heap, else moves the immediate out leaving `unit`.
- **Result.** `--jit-differential` **419 matched / 0 skipped / 722 native / 0 divergence / 0 leaks**.
  Bench `vm_jit/global_*` (top-level global `while` loop, interp vs forced JIT): **100k 4.46 ms →
  1.07 ms (~4.2×)** — less than the ~6× of a register-local loop (each global access is a memory
  indirect + an unbound/heap guard) but the scripting shape is now genuinely fast. New lang-vm
  native-global-loop test; the foundation test reworked (per-op bail leaves almost nothing a pure bail
  stub, so it now uses a string-returning fn — no fast op — to exercise the stub path).

> **Revisit at the end of the JIT arc — Finding-1 refinement (register-allocate uncaptured top-level
> locals).** The ~4.2× (vs a register loop's ~6×) is the cost of the global indirection. The
> next-gap doc's Finding-1 "later refinement" — the *compiler* promoting top-level `mut`/`let` that no
> nested `fn` captures into pure frame registers of prototype 0 (no global slot at all) — would make
> b_loop a plain register loop, native at the full ~6×, and speed the **interpreter** too. It is a
> compiler change, not a JIT one, and it touches destruction semantics (a top-level local is currently
> destroyed at program end in reverse `global_order`; as a register it dies at frame teardown — for
> `int`/`float` unobservable, but a heap top-level local with a `destruct` could shift timing vs the
> tree-walker → a differential risk to design against). Deferred deliberately: land it once the JIT
> arc is complete, so the two speedups (register promotion + native register loops) compose and the
> destruction-order work is done once, oracle-guarded. See `../perf/vm-throughput/next-gap-investigation.md`
> Finding 1.

## Open questions (resolve at sign-off)

- **Method JIT vs tracing.** Proposal: method JIT first (J1–J4), OSR/tracing deferred to J5+. Tracing
  would better handle megamorphic/branchy code but is a much larger harness.
- **Compile trigger & budget.** A simple call+back-edge counter with a fixed threshold; compilation is
  synchronous on the calling thread in v1 (background compilation is a later refinement).
- **Cranelift as a hard dep even when gated.** ✅ *Resolved in J0.* The `jit` feature pulls `cranelift-*`
  (29 crates); the default build pulls 0 (confirmed via `cargo tree`), and the sandbox/differential are
  byte-identical without it.
- **Debug/observability.** A `--dump-jit` (Cranelift IR / disassembly) analogous to `lang dump`, for the
  same agentic-debugging reasons.
- **Is the goal worth it?** The strategic call (§"Why this is worth a milestone"): general scalar-loop
  parity with a JIT engine vs investing in the design's proven strengths. Sign-off decides.
