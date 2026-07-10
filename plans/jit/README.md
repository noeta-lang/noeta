# JIT milestone (P-JIT) — closing the scalar/loop/call gap to a JIT engine

**Status: J0–J5 DONE + production-wired (integer/float/global/call/heap fast paths, leaf-op heap coverage, and OSR tiering; ~4–7× on native loops, ~2.3× on recursion, ~3.5–5.5× on top-level loops under real hot-counter promotion).** The VM-throughput arc (P-VMT: S0–S5 + RMW, GSLOT, CBR,
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
| **J3** | ✅ **DONE.** **Calls** — native `Op::Call` on the shared contiguous stack (no per-call allocation); heap-aware refcounting for the callee closure/results; **resume-native** re-enters the compiled caller after its callee returns; **native→native direct calls** — a compiled caller `call_indirect`s a compiled callee (value-returning `Return`, capacity-guarded so the register pointer stays valid), skipping the interpreter round-trip on the fast path, falling back to the shared setup otherwise. `fib`-class recursion **~2.3×**; loops ~6–8×. | Call ABI, stack/frame interaction, recursion depth. |
| **J4** | ✅ **LEAF-OP ARC DONE (slices 1–4).** **Heap & collections** — a generic `run_leaf_op` helper runs a single non-dispatching heap/collection op (the interpreter's exact arm, refcounts included) and returns "continue" or a resume pc; adding an op is a match arm. Slice 1: `MakeRange`/`IterSnapshot`/`ListLen`/`ListGet` → `for i in 0..n` native (**~2.6×**). Slice 2: `LoadField`/`SetField` (store factored into a shared `set_field_fast`, refcount-identical across tiers) — **812/813 protos native**. Slice 3: `Op::Index` (list/map/string subscript). Slice 4: `MakeTuple`/`TupleIndex` (completes `for (i,x) in xs.enumerate()` native). Heap-*dominated* loops are ~1.0–1.1× (each op is a helper call); the win is coverage — a loop mixing heap access with native arithmetic stays native. **Deliberately not leaf-ified** (documented below): dispatch-preceded constructors (`MakeMap`/`RequireMapKey`, `BuildString`/`Stringify`), thunk-running `MakeStruct`/`MakeEnum`. Genuinely fast heap access is the J5 inline cache, not more leaf ops. | **Refcount exactness** — the leak oracle is the gate. |
| **J5** | ✅ **DONE (OSR).** **Tiering polish + OSR** — on-stack replacement so a long-running loop enters tier 1 *mid-frame* at the loop header, not only at a call boundary. Closes the production hole where a top-level program that is one big loop (its `main` frame entered exactly once) never crossed the entry-count threshold and so ran entirely in tier 0 in production — every prior loop win was `force_jit`-only. Back-edge counting drives promotion; loop headers become native (re-)entry points (reusing the J3 resume-native mid-frame-entry machinery); OSR-capable prototypes compile `heap_aware` so mid-frame entry with a live heap value stays refcount-exact. **~3.5–5.5× on a top-level loop under real hot-counter promotion** (was tier-0-only before). Per-op bail + native-globals landed earlier (see below). | Deopt/OSR is subtle; do last. |
| **J6** | ✅ **DONE (negative result, documented).** **Native inline cache** — built the call-free native field read (guard on shape pointer + inline slot load, `#[repr(u8)]` + runtime-measured layout), proved correct/leak-free, and **reverted**: the field read is not the bottleneck (`heap_aware` store overhead + dependent-load latency are, both tier-independent). See the J6 section. | Field read was never the bottleneck. |
| **J7** | ✅ **DONE.** **Bare stores** — the J6 finding's real lever. A forward may-hold-heap dataflow (`heap_in_map`, fails closed) + an ownership-transfer peephole (`transfer_pairs`) elide the refcount work on register stores/`Move`/`Drop` wherever the value is provably immediate. Synergistic: `loop_jit/1M` ~7.4→6.0ms (~19%), `float_jit/1M` ~6.3→4.6ms (~28%). Blocked from going further by int→bigint overflow (accumulators are genuinely may-heap). Pure-numeric-loop lever; field loops unchanged (leaf helper dominates). See the J7 section. | Refcount exactness — leak oracle is the gate. |

| **J8** | ✅ **DONE (P-JSSA milestone — see [`ssa.md`](ssa.md)).** **SSA register promotion** — live VM registers held in Cranelift SSA values (typed/unboxed where provable, known-constant inlining), spilled only at bail/deopt/helper boundaries; the fast frameless call convention; universal residency (S5, heap values in SSA). Loop 10M **~29 ms** (target hit, ~2.3× over the slot codegen), fib(32) **~40 ms** (~4.3× vs pre-milestone main). | Spill-map/deopt correctness — oracle-gated per slice; anomaly + leak oracles green. |

Each slice: `--jit-differential` 0-divergence, leak residency 0 under forced JIT, a criterion
before/after, and `git` commit per green slice (standing directive).

**Production wiring — ✅ DONE (post-J4-slice-1).** Until now every win was oracle/bench-only: the
`lang` binary was built Cranelift-free and the JIT ran only under the oracle's `force_jit`. The shipped
CLI now enables the tier-1 JIT under ordinary hot-counter promotion, so real `lang run` / `lang test` /
`lang bench` accelerate. See the section below.

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

## J3 — what landed (native calls, foundation)

The deepest slice: native calls on the **shared contiguous register stack** (chosen over the simple
`call_value`-per-call path, which allocates per call and would make `fib` *slower*). It touches the
VM execution core, but reuses the interpreter's own logic so behaviour is identical by construction.

- **Shared call setup.** The `Op::Call` closure setup (reserve callee window, move args, fill
  defaults, carry upvalues, push the frame) was factored into `Vm::setup_closure_call`, used by both
  the interpreter arm and a new `jit_call` runtime helper. Native `Call` compiles to *one call to
  `jit_call`* (which reads the `Op::Call` back from `proto`/`pc`), then returns its outcome — so the
  call machinery lives in exactly one place and native calls push onto the **same** frame/register
  stacks (no per-call allocation). The ABI gained the `Vec<Frame>`/`Vec<Value>` handles for this; the
  return became an `i64` — a resume pc, `CALLED` (frame pushed), or `ABORTED`.
- **Heap-aware refcounting.** A call carries heap values in registers (the callee closure from
  `LoadGlobal`, results), so a **call-bearing prototype** switches to heap-aware codegen: `store_reg`
  releases the overwritten value, `LoadGlobal`/`Move` retain a moved heap value, `Drop` releases
  (destructor-aware when IR-relevant) — via `retain`/`release`/`release_value` helpers, all matching
  the interpreter's exact discipline. A **call-free** prototype (J1/J2/globals) keeps the faster
  refcount-free stores. Refcounts stay exact *across the tier-0/tier-1 boundary* — a native
  `LoadGlobal` retain is balanced by an interpreted overwrite, and vice versa.
- **Result — correctness proven, perf modest so far.** `--jit-differential` **419 matched / 0 skipped
  / 722 native / 0 divergence / 0 leaks** — the whole corpus's recursion, closures, and
  destructor-bearing calls run through native calls with exact refcounts. But the speedup is only
  **~1.1× on `fib`** (fib(24) 11.6 → 10.3 ms): a native `Call` **exits** the compiled function
  (`CALLED`), so the callee runs native (it enters at pc 0) but the **caller resumes in tier 0** after
  its first call. Each frame is native up to its first call + both subtrees native; the tail is
  interpreted. New `jit_recursive_call_is_native_and_correct` test + `vm_jit/fib_*` bench.
- **Resume-native (landed).** To keep the caller native after a call returns, the seam now fires at
  **every** frame `'reload` (not just `pc == 0`) and passes the frame's `entry_pc`; the compiled
  function has an entry-pc dispatch (a compare chain) that jumps to the resume block. The resume pcs
  (each `call_pc + 1`) are extra reachability roots, so their blocks get real code. So a whole `fib`
  frame now runs native except its final `Return` (a once-per-frame bail). This lifted `fib(24)` from
  ~1.12× to **~1.27×** with no loop regression (`--jit-differential` still 419/0/722-native/0-leak).
  One bug it surfaced and fixed: `StoreGlobal` was clearing its source register *before* its
  bail-on-heap-old-global check — harmless when the frame never ran that op natively (v1), but a lost
  value once resume-native reached it. The rule (now enforced): **a native op must decide its bail
  before mutating any state**, so a bail re-runs cleanly in the interpreter.
- **Native→native direct calls (landed).** The compiled caller now calls the compiled callee
  *directly*, skipping the interpreter round-trip on the fast path. Pieces: (1) a **value-returning
  `Return`** — a compiled `Return` calls `jit_return` (the shared `do_return` protocol) and yields
  `RETURNED`/`HALTED` instead of bailing, so the callee hands a value back to native code; (2)
  `jit_prepare_call` decides direct-ability at the call site (closure callee, plain arity, no
  upvalues, an already-compiled callee, and **stack capacity so `reserve_window` cannot reallocate** —
  which keeps the caller's register pointer valid across the call), sets up the callee frame, and
  returns its compiled entry pointer (else `0` → fall back to `jit_call`); (3) the caller
  `call_indirect`s that pointer on the shared stack and `jit_after_call` maps the callee's outcome —
  `RETURNED` → continue in place, anything else → point the callee frame at its pc and propagate
  `CALLED` so the interpreter finishes it (and resume-native picks the caller back up). `run`
  pre-reserves a generous register capacity so common recursion stays on the fast path; a deeper stack
  reallocates once and the capacity check re-passes — performance only, never correctness.
- **Result.** `fib(24)` **~1.27× → ~2.24×**, `fib(28)` ~2.3×, with loops unchanged (`--jit-differential`
  419/0/768-native/0-leak). The remaining distance to loop-like speedup is the per-call setup/refcount
  work that recursion inherently pays; the interpreter round-trip is gone.

## J4 — what landed (slice 1: heap ops via one generic helper)

The heap-op mechanism is deliberately uniform: a single **`jit_run_leaf_op`** helper runs one
non-dispatching heap/collection op — the interpreter's *exact* arm, refcounts and all — on the shared
register stack, and returns `OUTCOME_CONTINUE` (done, advance to `pc + 1`) or the op's own pc (it
can't handle this instance: a receiver that would dispatch to a user method, or a case that would
raise — the interpreter re-runs it). **Every early return is before any register write**, so a re-run
starts clean (the same rule J3's `StoreGlobal` fix established). The codegen for any such op is one
shared `emit_leaf_op` (call the helper, continue or propagate); a prototype containing one is
`heap_aware`, so the surrounding native writes stay refcount-correct. **Adding an op is a match arm
plus a name in `is_leaf_heap_op` — no new helper, no new codegen.**

- **Slice 1 — `for i in 0..n`.** Lit up `MakeRange` / `IterSnapshot` / `ListLen` / `ListGet`, so the
  idiomatic range loop is native (its `IterSnapshot` still bails on a user `Iterable`; a plain range
  never does). `vm_jit/forrange_*` (interp vs forced JIT): **100k 3.63 → 1.74 ms (~2.1×), 1M 38.7 →
  14.9 ms (~2.6×)** — below the `while` loop's ~6× because a range materializes an `n`-element list and
  `ListGet` is a per-iteration helper call, but the idiomatic loop now goes native. `--jit-differential`
  419/0/768-native/0-leak; new `jit_for_range_loop` test.
- **Slice 2 — field access (`LoadField` / `SetField`).** `LoadField` resolves the slot without the
  interpreter's loop-local inline cache (`slot_of` then `slot_at`, bailing where the interpreter would
  raise). `SetField`'s store — the reference-`class` in-place mutation, the value-`struct` COW, and the
  `reuse` fast path — is factored into a shared `Vm::set_field_fast` that **both** the interpreter arm
  and the leaf helper call, so their refcount behaviour is identical by construction (not by duplicated
  code); the field-not-found path performs no mutation, preserving bail-before-mutate. Coverage jumps to
  **812/813 protos native**. The field-loop micro-bench is only ~1.1× (each `LoadField`/`SetField` is a
  full helper call — a `match` + name-intern + slot scan — roughly matching interpreter dispatch, and a
  field-dominated loop is almost all field ops); the win is real where field access is mixed with
  heavier native arithmetic, and the coverage is near-total. A genuinely fast field load wants a native
  inline cache (guard on the receiver's shape pointer, load at the cached slot offset) — a J5-class slice.
- **Slice 3 — subscript indexing (`Op::Index`).** The non-dispatching list / map / string paths run in
  the leaf helper (bounds-checked list, string-keyed map, char-indexed string), retaining each looked-up
  element exactly as the interpreter does. A user `Index` impl (`o[i]` → `get`) pushes a frame and bails;
  every error case (out-of-bounds, wrong index type, missing key, non-indexable) bails so the interpreter
  raises the exact diagnostic — all before any register write. The micro-bench is **roughly break-even**
  (~1.0×): an index-dominated loop is almost all `Index` ops, each a helper call about as costly as the
  interpreter's inline arm, and `run_module_jit` folds in compile time. The value is that a loop *mixing*
  indexing with native arithmetic now stays native across the `Index` instead of bailing (and
  tier-bouncing) every iteration; in production the hot-counter only compiles genuinely hot code, where
  the compile cost amortizes.
- **The honest pattern for J4 leaf ops.** Field access and indexing confirm it: a leaf op that is a
  helper call per instance gives near-total *coverage* but marginal per-op *speedup* on heap-dominated
  loops — the real JIT wins stay in native arithmetic/control (loops 6–7×, globals 4–5×, calls 2.3×).
  Making heap access itself fast wants a native inline cache (guard on the receiver shape, load at the
  cached offset / probe the cached bucket) — a J5-class slice, not more leaf ops.
- **Slice 4 — tuples (`MakeTuple` / `TupleIndex`).** `MakeTuple` retains its elements into a fresh
  tuple (no bail path — construction never fails). `TupleIndex` is the positional projection `receiver.N`,
  retaining the element into `dst` and bailing on an out-of-range index (unreachable for well-typed
  code). Together they make `for (i, x) in xs.enumerate()` fully native — `enumerate` yields `(int, T)`
  tuples via the already-native `ListGet`, and the destructuring reads them with `TupleIndex`.
- **Leaf-op arc closed here — what is deliberately *not* leaf-ified, and why.** The mechanism is
  proven, but the remaining ops give no perf and add real risk:
  - **`MakeMap`** is always preceded by per-key **`RequireMapKey`** validation ops, which bail; the
    native code never reaches `MakeMap`. **`BuildString`** is preceded by **`Stringify`** ops that
    dispatch a `Display` object to `to_string` (a frame push) and bail. Leaf-ifying the target op alone
    buys nothing while its predecessor still bails — and both are allocation-bound, where native
    execution saves nothing anyway.
  - **`MakeStruct`** / **`MakeEnum`** run field-default **thunks** (`run_thunk`) that execute arbitrary
    code and can **abort after** the op has already retained values into slots — that violates
    bail-before-mutate and would need abort-propagation plumbing through the leaf helper, for an
    allocation-bound op with no native upside.
  - **Dispatching ops** (`CallMethod`, object `Index`→`get`, `Display` `Stringify`) push frames and stay
    bail points by design.
- **The real next lever is J5, not more leaf ops.** Field access and indexing are the common heap-hot
  paths and both are helper-bound at ~1.0–1.1×. Making them genuinely fast needs a **native inline cache**
  — guard on the receiver's shape pointer and load the field at the cached slot offset (or probe the
  cached map bucket) in native code, no helper call. That is a J5-class slice with its own deopt story,
  not a leaf-op addition.

## Production wiring — what landed (JIT reaches `lang run`)

The tier-1 JIT is no longer oracle-only. The shipped `lang` binary enables it under ordinary
hot-counter promotion, so real programs accelerate without any flag.

- **`lang-cli` defaults to the `jit` feature** (`default = ["jit"]` → `lang-vm/jit`). The shipped binary
  pulls Cranelift and JITs hot code; `cargo build --no-default-features` still yields an
  interpreter-only, Cranelift-free binary with byte-identical behaviour (the feature gate stays real).
- **Only the real-host paths arm the JIT.** The three `run_module_with_host*` entry points call
  `init_jit()` with `force_jit = false`, so a prototype interprets until it has been entered
  `JIT_HOT_THRESHOLD` (50) times, then compiles — short scripts pay no compile cost, hot loops/recursion
  go native. The sandbox `run_module` / `try_run` paths pass `jit = false`: they are the
  `--jit-differential` oracle's pure tier-0 baseline, and worker isolates stay tier-0 (Cranelift's
  `JITModule` is `!Send`).
- **Measured end-to-end** on a JIT-vs-`--no-default-features` release binary, a nested loop-with-calls
  workload (`sum_to` in a hot outer loop): **~1.65 s → ~0.18 s, roughly 9×**. Correct output either way.
- **Gates.** All 50 CLI integration tests + the doc-sample gate pass with the JIT default-on (real
  isolates, the `@test` runner, backpressure all route through it). CI keeps the library + oracle + docs
  jobs Cranelift-free (excluding `lang-cli`), adds a lean `--no-default-features` CLI build, and gates
  the JIT-enabled CLI (integration + doc samples) in the `jit` job.

## J5 — what landed (OSR / on-stack replacement)

The production hole J5 closes is specific and real: **hot-counter promotion counted only frame
*entries*.** A program that is one long top-level loop — `mut i = 0; while i < N { … }` at module
scope — runs entirely inside `main` (prototype 0), which is entered exactly *once*. Its counter
reached 1, never `JIT_HOT_THRESHOLD` (50), so **it never compiled in production** and ran every
iteration in tier 0. Every top-level-loop win recorded above (native globals ~4.2×) was measured under
the oracle's `force_jit`; a real `lang run` of that shape got nothing. OSR fixes exactly this.

- **Back-edge counting drives promotion.** The tier-0 inner loop now treats a **taken backward
  branch** (`Jump`/`JumpIf*`/`CondBranch` whose `target <= pc`) as a loop back-edge: it bumps the same
  per-prototype counter frame entry uses and, once hot, compiles the prototype and re-enters native at
  the loop header. The check is gated on `self.jit.is_some()`, so the deterministic sandbox / worker /
  no-feature paths (where `jit` is `None`) pay a single predicted branch and never OSR — the
  differential baseline is untouched.
- **Loop headers are native (re-)entry points.** `entry_pcs` now includes every backward-branch target
  alongside pc 0 and the J3 post-call resume pcs; the compiled body gets an entry-pc dispatch arm and a
  reachable block for each, so native execution can *begin* at a loop header. This reuses the exact
  mid-frame-entry machinery J3 resume-native built — an OSR entry is just "re-enter this frame at pc P"
  where P is a loop header instead of a `call_pc + 1`.
- **Refcount-exact mid-frame entry.** A prototype with any OSR entry compiles `heap_aware`
  unconditionally: OSR begins execution with whatever the interpreter left in the registers — a heap
  value may be live — so every register store must release-the-old / retain-the-moved, exactly the
  precondition a call-bearing prototype already forces. A pure-int loop forced `heap_aware` is
  unaffected (the extra `is_pointer` checks are no-ops on immediates). The `--jit-differential` leak
  oracle gates this: **419 matched, 0 divergence, 0 leaks, 812/813 native**, unchanged — the new
  loop-header blocks and forced-`heap_aware` stores are correct on every corpus program.
- **One OSR per prototype.** If a prototype is already compiled, a tier-0 back-edge does *not* re-OSR:
  the frame goes native at its next `'reload` anyway, and re-entering from tier 0 after a native op
  bailed back would risk bouncing tier-0↔tier-1 every iteration for a loop native can't sustain.
- **Result.** New `run_module_jit_hot*` harness (ordinary hot-counter promotion, not `force_jit`) +
  two lang-vm tests: a top-level integer loop entered once goes native via OSR, and a top-level
  heap-body loop (a struct read/written each iteration, a heap value live at the OSR point) matches the
  interpreter. Bench `vm_jit/osr_*` (top-level global loop, interpreter vs **hot-counter** JIT — the
  production path, where before OSR the JIT column would equal the interpreter): **100k 4.43 → 1.27 ms
  (~3.5×), 1M 49.6 → 8.97 ms (~5.5×)**.

**Deliberately *not* built here — the native inline cache.** The J4 narrative flagged a "native inline
cache" (guard the receiver's shape pointer, load the field at a cached slot offset in native code, no
helper call) as the real lever for *heap-access* speed. It is **not** sound to build today: the heap
object's `Payload` is a plain Rust enum and its slots are a `Vec`, neither of which has a layout the
JIT may inline into machine code — a `rustc` upgrade that reordered the enum or changed `Vec`'s
internals would silently corrupt memory under JIT with no compile-time guard. A sound version needs a
layout-locked accessor from `lang-value` (a `#[repr(C)]` fast header, or an ABI the codegen can target)
first. Deferred to a future slice; it is orthogonal to OSR (a tiering change, not a heap-layout one),
which is why J5 is OSR — the highest-value *sound* tiering win — rather than the inline cache.

## J6 — the native inline cache (built, measured, reverted — the field read is not the bottleneck)

The J4/J5 notes both point at a "native inline cache" for field access as the remaining heap-hot lever.
The full investigation — helper-internal cache, then a real call-free native read via `#[repr(u8)]` +
a runtime-measured object layout — found it **does not help**: the bottleneck is `heap_aware` store
overhead and memory latency, not the `LoadField` path. Both attempts were reverted; this section is the
record so the effort is not repeated, and points at the real lever (bare stores for immediate values).

**What field access costs in the JIT today.** A `LoadField` runs through the generic `jit_run_leaf_op`
helper: a native `call` out of the compiled loop, reconstitute `&mut Vm`, a `match` over the leaf-op
set, then `slot_of` (a **field-name string scan** over the shape's fields) + `slot_at` + retain +
store. The tier-0 interpreter's `LoadField` is a *call-free* `match` arm **with** an inline cache
(shape-pointer-keyed, skips the scan). So on a field-read-dominated loop the JIT is actually **slower
than the interpreter** — the per-`LoadField` helper *call* is the floor, and the interpreter has no
call.

**The shortcut that does not help — a helper-internal inline cache.** The natural idea: give the JIT
leaf helper its own shape-keyed cache (a VM-owned array indexed by each op's existing `cache` slot, the
tier-0 mechanism) so monomorphic `LoadField`s skip the `slot_of` scan. Built and **measured** (bench
`vm_jit/widefield_*`, a read-only loop over an 8-field struct reading the last field — worst-case scan):

| | 100k | 1M |
|---|---|---|
| interpreter (tier 0, cached) | 3.77 ms | — |
| JIT, scan every access | 4.58 ms | 41.5 ms |
| JIT, helper-internal inline cache | 5.26 ms | 40.8 ms |

The cache is a **wash-to-slight-loss**: the shape-pointer guard (`object_shape_ptr` → decode +
`with_payload` + `Rc::as_ptr`, plus the cache-array lookup) costs about as much as the short
field-name scan it replaces, and *neither moves the floor* — the JIT stays above the interpreter
because the helper call dominates both. Reverted; only the reframed comment and the `widefield_*` floor
bench remain. The lesson: no optimization *inside* the helper can win, because the helper *call* is the
cost.

**The call-free native read — built, measured, and it does NOT win either.** The obvious next step is
to read the field **without a call**: decode the receiver, guard its shape pointer against a per-site
cached shape (inline compare), and on a hit load the slot at the cached offset — all as machine code.
The blocker was thought to be that the `Payload` **Rust enum** has an unspecified layout. That turned
out to be surmountable *without* a heap re-architecture: `Payload::Object` is fully encapsulated in
`lang-value`, and a `#[repr(u8)]` on `Payload` gives it a defined layout (RFC 2195), so a
`lang_value::object_layout()` probe can **measure** the payload / discriminant / shape / slots offsets
(plus the `Vec`'s runtime-measured data-pointer offset — the probe *caught* that `Vec` does not store
its pointer first on this build), lock them with a test, and let the JIT bake the runtime-measured
offsets into code generated in the same process. This was actually implemented: `#[repr(u8)]` +
`object_layout()` + a lock test, and an `emit_load_field_native` that decodes, tag-checks, shape-word
guards, and loads the slot inline (only a `jit_field_slot` helper on a cold miss). It is **correct and
leak-free** — `--jit-differential` 419 / 0-divergence / 0-leak with the native reads on every corpus
program.

**But it produces no speedup**, so it was reverted. The decisive A/B (bench `vm_jit/mixed_*`, a loop
mixing several field reads with heavy native arithmetic — the case that should most favour staying
native):

| mixed loop, 100k | time |
|---|---|
| interpreter | 11.6 ms |
| JIT, leaf-helper `LoadField` | 12.3 ms |
| JIT, **native-IC** `LoadField` | 12.8 ms |

The JIT is **slower than the interpreter regardless of the `LoadField` implementation**, and the native
IC is no better than the leaf helper. So `LoadField` was **never the bottleneck**. Two tier-independent
costs dominate a field-bearing loop: (1) any prototype touching a heap value compiles `heap_aware`, so
**every register store** — including all the arithmetic temporaries — pays a load-old + `is_pointer`
branch (and the interpreter's `set_reg` pays the same release-check), and (2) the field read is a chain
of **dependent memory loads**, latency-bound in either tier. Neither is touched by making the read
native. This closes the "native inline cache is the heap-perf lever" hypothesis with hard data — the
same verdict as the helper-internal cache, one level deeper.

**The real lever is the `heap_aware` store overhead, not the field read.** A pure-integer loop is ~6×
in the JIT; adding one field read drops it *below* the interpreter, because the whole prototype flips
to refcount-checked stores. The promising direction is to keep stores **bare** where the JIT can prove
the overwritten value is an immediate — a codegen refinement in `lang-jit`, not a heap-layout change.
That is **J7** (below).

## J7 — bare stores (elide refcount work where the value is provably immediate)

The J6 finding's "real lever," built and shipped. A `heap_aware` prototype made **every** register
store/`Move`/`Drop` refcount-correct (load-old + `is_pointer` release, `is_pointer` retain), even for
the arithmetic temporaries that never hold a heap value. That scaffolding — a load + a
never-taken branch per op, ~3–4× the real work — is what pins a field-bearing loop below the
interpreter and taxes even pure-arithmetic loops.

**What confirmed the hypothesis.** Forcing every store bare (an unsound A/B, valid only for timing a
loop with no heap in registers) cut `loop_jit/1M` 7.4ms → 5.7ms (~23%). So the store overhead is real
and worth attacking — but a *sound* version must keep the releases wherever a value genuinely may be
heap.

**The blocker: int overflow.** `int` is arbitrary-precision — `a + b` past the 48-bit immediate range
heap-boxes a big int (`Value::int(i64)`). So an accumulator/counter register is *genuinely* may-heap:
its Drop and its `Move`'s retain cannot be elided. A pure heap-analysis therefore leaves the hot
accumulator ops in place and, measured alone, is a wash.

**Two sound, composable refinements** (both in `lang-jit`, gated by the leak-under-JIT oracle):

- **`heap_in_map`** — a forward may-hold-heap dataflow over the tier-0 CFG, seeded with the parameters
  possibly-heap at entry. Per store site it says whether the overwritten value may be a heap pointer;
  where it provably cannot (a fresh/`Drop`-ped slot, a bool, an immediate arithmetic result) the store
  is bare. It **fails closed**: any unmodeled op opts the whole prototype out (all-true = prior
  behavior), and modeling is confined to the pure-arithmetic-loop subset, so a call or leaf op keeps
  the old discipline. The fixpoint's in-set at a loop header over-approximates exactly what tier-0 can
  leave there, which is why a call-free `heap_aware` prototype (native runs once per activation, so an
  OSR header / `pc 0` is the only place the interpreter can have left a live register) is sound; a
  call-bearing prototype contains `Op::Call`, which is unmodeled, so it stays all-true.

- **`transfer_pairs`** — an ownership-transfer peephole: `Move dst <- src` immediately followed by
  `Drop src` is a transfer whose retain (in the `Move`) and release (in the `Drop`) cancel *exactly*,
  regardless of the value's type. Both are emitted bare. This is what removes the retain+release on the
  transient arithmetic temporaries that the overflow-to-bigint semantics keep may-heap — the ops the
  dataflow alone cannot touch. Sound by construction (refcount-neutral algebra); guarded so a `Drop`
  that is a branch target (reachable without its `Move`) is left alone.

**The two are synergistic.** Measured on the register-local integer loop, the dataflow alone is a wash
and the peephole alone is a wash (either leaves enough scaffolding to bottleneck the dependency chain),
but **together** they cut `loop_jit/1M` ~7.4ms → ~6.0ms (~19%) and `float_jit/1M` ~6.3ms → ~4.6ms
(~28%, floats are never heap so more of the loop goes bare). The residual vs the unsound fully-bare
build is the genuinely-needed release of the previous accumulator/counter value (a possible big int) —
correct, not overhead.

**Scope, honestly.** This is the pure-numeric-loop lever, not the heap one: the field-read floor is
unchanged (the leaf-op helper dominates there, exactly as J6 found — `field_jit` is a wash under
forced-bare). Field-bearing loops remain best served by the interpreter until a call-free native field
read lands. Validation: `--jit-differential` 419/0-divergence/0-leak, plus unit tests locking the
analysis contract (`heap_in_map`/`transfer_pairs`).

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
