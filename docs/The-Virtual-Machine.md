# The Virtual Machine

How the bytecode backend executes a program: a register machine over NaN-boxed values, a shape-based object model, and inline caches on every property access.

## A register-based bytecode VM

The VM (`noeta-vm`) is a Tier-0 **register machine** (Lua/Dalvik style), not a stack machine. Register bytecode issues fewer dispatches per operation than a stack VM and is a friendlier base for the Tier-1 JIT that compiles hot prototypes to native code ([below](#tier-1--the-jit)).

The compiled artifact (pure data in `noeta-bytecode`):

- **`Op`** — the opcode set: arithmetic and branching, `Call`/`Return`, `MakeList`/`MakeMap` and iteration ops, `CallBuiltin`/`LoadNativeFn` for native calls, the object-model ops (`MakeStruct`/`MakeEnum`/`LoadField`/`CallMethod`), the memory-management ops (`MakeStructInPlace` for in-place reuse, the drop ops for RC), and the concurrency ops (`MakeChannel`/`SpawnIsolate`).
- **`Chunk`** — one function prototype: its `code`, constant pool, `num_params`, `num_registers`, and `frame_locals` (destructor-teardown pins). `Chunk::disassemble` gives stable text for snapshot tests.
- **`Module`** — the prototype table (proto 0 is the top-level program; the rest are functions/closures/methods), the flat `shapes` layout table, the `methods` dispatch table, and `cache_slots` (the inline-cache count).

Execution is frame-based: each call pushes a `Frame` with its own register file, program counter, and return slot. Register 0 of a method frame is the receiver. Top-level bindings and function names live in a by-name global environment. Notably, the dispatch loop's frame stack is a *Rust local*, not VM state — so a native builtin like `map`/`filter` re-enters the VM by running a fresh frame stack to completion (ordinary Rust recursion over the shared globals/stdout).

### Register allocation

IR→bytecode first allocates registers *monotonically* (every temporary and local gets a fresh slot living to teardown); a bytecode→bytecode post-pass (`noeta-compiler`'s `regalloc::coalesce`) then reclaims the waste by graph coloring:

1. **Liveness** — a backward dataflow to a fixpoint over the real control-flow graph.
2. **Interference graph** — a definition interferes with everything in its `live_out`, plus a def↔use edge forbidding one op's source and destination sharing a slot.
3. **Greedy coloring** — parameters pre-colored to `0..num_params` (the calling convention addresses them by index); `num_registers` shrinks to the color count.

The payoff is both smaller per-activation register arrays and *prompt* reclamation: when a later value reuses a slot, the write releases the previous dead occupant right there instead of at teardown. Destructor-bearing locals are pinned so a panic-teardown drop never loses its value to coalescing.

## NaN-boxing

Every runtime value is a single 64-bit word — `struct Value(u64)` in `noeta-value`. Doubles are stored natively; everything else is encoded in the payload of a quiet NaN with a small type tag, and heap pointers live in the low 48 bits:

```text
float      : any bits where (bits & QNAN) != QNAN     (a real double)
pointer    : SIGN_BIT | QNAN | addr48                 (a refcounted heap object)
small int  : QNAN | INT_TAG | payload48 (sign-ext)    (immediate, ±2^47)
f32        : QNAN | F32_TAG | bits32                   (immediate packed f32)
unit/bool  : QNAN | TAG_{UNIT,FALSE,TRUE}             (immediate singletons)
pending    : QNAN | TAG_PENDING                        (the async-suspend sentinel)
```

Why: a fat `enum Value` would be 16+ bytes and branchy; NaN-boxing keeps values pointer-sized and cache-friendly (the LuaJIT / JavaScriptCore approach), which matters on the hot dispatch path. An `i64` too large for the 48-bit immediate is boxed on the heap as `Payload::Int`, so full-width integers still work.

A heap pointer refers to an `Obj` = a `#[repr(C)]` header + a `Payload`. The payload variants cover everything heap-allocated: `Str`, `Bytes`, boxed `Int`, `Closure { proto, upvalues }`, `Cell` (a mutable upvalue cell), `List`, `Tuple`, `Set`, `Map`, `PackedList { schema, bytes }` (a flat unboxed numeric buffer), shaped `Object`/`Enum`, `Extern` (the generic registered-extension leaf that file handles, UUIDs, and other native types now share), channel `Sender`/`Receiver`, and iterator state. The header carries the `refcount`, a monotonic creation `seq` (for deterministic destruction tie-breaks), the cycle collector's `color`/`buffered` flags, and the isolates `shared` bit.

> [!NOTE]
> **`noeta-value` is the one crate whose core value model uses `unsafe`** — the NaN-box pointer round-trip and heap-header access. It opts out of the workspace's `unsafe_code = "forbid"` lint and is miri-gated. It also emits the refcount primitives and the operator semantics (`apply_binary`, `structural_compare`, …) that *both* backends share, so structural equality and ordering cannot diverge. The compiler crate is itself `unsafe`-free; the VM crate denies `unsafe` by default (`unsafe_code = "deny"`, not the workspace's `forbid`) with a few explicitly-`#[allow]`ed sites at the Tier-1 JIT's native-ABI boundary (reconstituting VM state from the Cranelift-supplied pointers, the fast call convention's register-stack reservation) — everything else in both crates stays `unsafe`-free.

## Shapes (hidden classes)

Objects are not per-instance hashmaps. Each aggregate points to a **shape** (a V8/JSC-style hidden class) describing its layout; the fields live in a flat inline slot array indexed by the shape. This is `noeta-object`, pure layout data below `noeta-value` in the DAG.

- A `Shape` records the type name, the ordered slot names, and enum-variant info; `ShapeKind` is `Struct`/`Class`/`Opaque`/`Enum`; `slot_of(name)` resolves a field name to a slot index.
- The compiler emits a flat shape table into the `Module`; the VM wraps each entry in an `Rc<Shape>` once at startup and clones that handle into every value of that shape — so **shape identity is a cheap pointer comparison**, which is exactly what the inline cache keys on.
- Field access is: read the `Rc<Shape>`, `slot_of(name)` → index, index the flat slot array. A structural update (`Money { amount: 300, ..a }`) copies slots shallowly under the same shape.

Beyond speed, a shared shape gives per-instance type identity cheaply — which is how **generics fall out**: a `Box<User>` is a shape carrying a type-parameter slot, the per-instance type identity a dynamic language usually lacks.

## Inline caches

Every property-access and method-call site caches the last shape it saw and the resolved slot/prototype, so a repeated monomorphic access skips the lookup.

- The compiler assigns each `LoadField`/`CallMethod` op a cache slot id via a module-global counter (→ `Module.cache_slots`).
- The VM allocates a per-run side array `Vec<Option<(&'static Shape, u32)>>` sized to `cache_slots` — a local in the dispatch loop, so it neither borrows `self` across the loop nor leaks between runs.
- **Hit**: the receiver's shape pointer equals the cached one → use the cached slot/prototype directly (a raw pointer compare, no refcount bump).
- **Miss**: resolve via `slot_of` (field) or the `(type, method)` hashmap (method), then refresh the entry — the cached key is an interned `&'static Shape`, so it can never alias a freed shape.

`LoadField` caches the field slot index (skipping the name scan); `CallMethod` caches the method prototype (skipping the hashmap lookup *and its two string clones*, the dominant cost). Shapes are immutable once created, so a cached `(shape, slot)` never goes stale — a different shape simply misses and refreshes. The cache is VM-only and invisible to `RunResult`, so the differential is unaffected; measured impact is roughly −22% on member dispatch.

## Tier 1 — the JIT

Everything above is **Tier 0**: the interpreter dispatch loop. On top of it sits an optional **Tier 1** — a method-at-a-time [Cranelift](https://cranelift.dev/) JIT (`noeta-jit`) that compiles a hot prototype to native machine code. It lives behind a `jit` cargo feature: the default `noeta` binary enables it, but a `--no-default-features` build pulls in *zero* Cranelift crates and runs Tier 0 only, byte-for-byte identically. The sandbox, the conformance corpus, and isolate worker threads always run Tier 0 (the JIT's `JITModule` is `!Send`), so the differential oracle's baseline never involves native code.

### The design in one line

**The fast path is native; everything else calls back into the interpreter's own code.** Integer/float arithmetic, comparisons, branches, `LoadConst`/`Move`, global-slot access, and calls compile to real Cranelift IR. Anything richer — a heap/collection op, an uncompiled callee — is a call to a *runtime helper* that runs the interpreter's exact arm (refcounts included). So Tier 1 can never *disagree* with Tier 0: the parts it doesn't specialize, it delegates, and the parts it does are guarded.

### The shared stack makes deopt free

A compiled prototype runs on the **same contiguous register stack** the interpreter uses, with the ABI `fn(vm, regs, base, globals, frames, regs_vec, entry_pc) -> i64`. Because native code reads and writes the interpreter's real registers, **deoptimization costs nothing to set up**: when native code reaches an op it doesn't compile — a `Return`, or a guard that fails (an operand isn't a small int, an add overflows the 48-bit immediate range, an `if` condition isn't a bool) — it simply *returns the bytecode pc of that op*, and the interpreter resumes there over the already-correct register window. Guards always **bail before mutating any state**, so the interpreter re-runs the op cleanly. This guard-and-bail contract is what lets an untyped bytecode be compiled speculatively without a separate deopt-state map.

### Registers live in SSA (mem2reg)

Within a compiled prototype, VM registers are **Cranelift SSA variables**, not memory:

- **Registers live in machine registers for the whole native region** — heap values included. The in-memory register stack is touched only at region boundaries: entry loads, bail-edge spills, helper syncs.
- **Typed values run unboxed.** A forward kind dataflow proves registers `Int`/`Bool`/`Float` along native paths, and a second *raw* (unboxed) variable per register lets typed arithmetic skip the NaN-box tag checks and box/unbox chains entirely — a counting loop's governing compare compiles to literally one `cmp; jl`.
- **Claims are verified at entries, never trusted.** Every mid-frame entry (a resume after a call, an OSR loop header) checks the claimed registers against what Tier 0 actually left in the slots and bails on a violation — Tier 0 can heap-box an overflow exactly where the native path would have bailed first, and a wrongly-trusted claim would corrupt refcounts.

This "claims verified at entries, maintained by native defs" contract is what lets an untyped bytecode carry unboxed values safely.

### Getting hot, and getting in mid-loop (OSR)

Promotion is a per-prototype counter: a prototype crossing `JIT_HOT_THRESHOLD` frame entries **or loop back-edges** is compiled. The back-edge trigger is **on-stack replacement (OSR)** — a long-running loop enters Tier 1 *at its loop header*, mid-frame, rather than only at the next call. Without it a top-level program that is one big loop (its `main` frame entered exactly once) would never get hot; with it, loop headers become native re-entry points, reusing the same mid-frame-entry machinery that re-enters a compiled caller after a call returns.

The whole tier-transition loop, in one picture:

```text
              ┌──────────────────────────┐
              │   Tier 0 — interpreter   │◄──────────────────────────┐
              └────────────┬─────────────┘                           │
       frame entries / loop back-edges                               │
        cross JIT_HOT_THRESHOLD                                      │
                           ▼                                         │
                 compile the prototype                               │
                           │                                         │
            ┌──────────────┴──────────────┐                          │
            ▼                             ▼                          │
   native entry (next call)   OSR entry (hot loop header,            │
            │                        mid-frame)                      │
            └──────────────┬──────────────┘                          │
                           ▼                                         │
          native code runs on the interpreter's                      │
                  own register stack                                 │
                           │                                         │
      a guard fails / an uncompiled op is reached                    │
                           │                                         │
                           ▼                                         │
     normalize the register window, return the bytecode pc ──────────┘
             (Tier 0 resumes at exactly that op)
```

### Calls stay native — the fast call convention

A native `Call` first consults a **per-call-site inline cache**. On a hit (same callee closure as last time — the cached closure is pinned so bits-equality proves identity), the entire call setup is native:

- **Zero helper calls on the hot path.** Native code capacity-checks the stacks, extends the register stack over an **uninitialized** callee window, writes the frame record from a baked template, and calls the callee's **fast-convention body**: arguments travel as machine arguments, the result comes back as a return value, and the return protocol (masked slot releases, frame pop, stack truncation) is emitted inline from the baked frame layout. Recursive `fib` runs frame-to-frame in native code.
- **Every native exit normalizes the window.** The soundness contract is that the interpreter must never see the uninitialized window: before the interpreter — or an abort's unwind — can look, native code spills the live and heap-desynced registers and unit-fills the rest.
- **The frame stack stays fully honest.** A cache miss or an un-direct-able callee falls back to a helper that pushes the callee frame for the interpreter; either way every call pushes a real frame, which is what keeps deopt and unwinding trivial.

### Refcounts across the tier boundary

A prototype that keeps a heap value in a register is compiled **heap-aware**: overwriting a possibly-heap register releases the old value (straight from its SSA variable — no load) and moving one retains it, matching the interpreter's `set_reg`; where a dataflow proves a value immediate, all of that refcount work is elided. Because a heap value's slot can be out of sync with its variable (holding a released pointer, or missing the reference the variable owns), a *slot-hazard* dataflow extends every sync point's spill set so teardown and unwinding always see an ownership-exact window.

### The oracle

Tier 1 has its own gate, separate from the eval↔VM differential: **`--jit-differential`** runs every corpus program through the interpreter and through the forced-Tier-1 JIT and asserts three things at once:

- **A byte-identical `RunResult`** — the JIT may never change observable behavior.
- **Zero heap residency** — no program leaks under native code.
- **Zero refcount anomalies** — during cycle collection, every unreachable object's refcount must equal its in-edges from the garbage set (unreachable garbage can only reference itself). This closes a blind spot the residency check alone has: a *skipped* retain or release is caught even when teardown's backup sweep would have absorbed the orphan — exactly the failure mode of a wrong immediacy claim, and the check that made a latent mid-frame-entry bug reproducible (it also caught three unrelated interpreter refcount bugs on arrival).

The oracle has a **second arm**, `--jit-differential --cancel-poll`, which runs the same corpus with a never-set cancellation flag armed on the JIT side. That is not a knob: a run that can be cancelled gets genuinely different native code — a cancellation poll at every loop header (see [Concurrency Internals](Concurrency-Internals#the-third-safepoint-a-jit-loop-header)) — and the two shapes are covered separately because testing either alone would leave the other unchecked.

Because refcount exactness is the thing most likely to drift when native code manages the heap — and JIT-generated code cannot run under miri — these checks are as load-bearing as the output check. A historical snapshot from when this coverage was measured: 433 programs, 0 divergences, 0 leaks, 0 anomalies, and 893 of 894 prototypes compiled to real native code; the conformance corpus has grown substantially since, so treat those numbers as a snapshot, not a live count.

> [!NOTE]
> **One negative result is worth recording.** A call-free *native inline cache* for field reads was built (guard the receiver's shape pointer, load at a cached slot offset) and **reverted** — measured no faster than the leaf-op helper. Field-bearing loops are bottlenecked by the dependent-load latency of the read and the heap-aware store discipline, both tier-independent, not by the field lookup. The JIT's wins are in native arithmetic, control flow, calls, and OSR'd loops; heap-dominated loops are best left to the interpreter's own inline cache.

## See also

- [Memory Management](Memory-Management) — the refcount discipline the VM's registers and heap follow, and what the JIT reproduces across the tier boundary.
- [Performance Techniques](Performance-Techniques) — inline caches, layout, the JIT, and what was measured.
