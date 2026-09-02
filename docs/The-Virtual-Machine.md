# The Virtual Machine

How the bytecode backend executes a program: a register machine over NaN-boxed values, a shape-based object model, and inline caches on every property access.

## A register-based bytecode VM

The VM (`noeta-vm`) is a Tier-0 **register machine** in the Lua/Dalvik style. Register bytecode issues fewer dispatches per operation than a stack machine does, and it is the base the Tier-1 JIT compiles hot prototypes from ([below](#tier-1--the-jit)).

The compiled artifact (pure data in `noeta-bytecode`):

- **`Op`**, the opcode set: arithmetic and branching, `Call`/`Return`, `MakeList`/`MakeMap` and iteration ops, `CallBuiltin`/`LoadNativeFn` for native calls, the object-model ops (`MakeStruct`/`MakeEnum`/`LoadField`/`CallMethod`), the memory-management ops (`MakeStructInPlace` for in-place reuse, the drop ops for RC), and the concurrency ops (`MakeChannel`/`SpawnIsolate`).
- **`Chunk`**, one function prototype: its `code`, constant pool, `num_params`, `num_registers`, and `frame_locals` (destructor-teardown pins). `Chunk::disassemble` gives stable text for snapshot tests.
- **`Module`**, the prototype table (proto 0 is the top-level program; the rest are functions/closures/methods), the flat `shapes` layout table, the `methods` dispatch table, and `cache_slots` (the inline-cache count).

Execution is frame-based. Each call pushes a `Frame` with its own register file, program counter, and return slot, and register 0 of a method frame is the receiver. Function names, imported modules, and the top-level bindings something outside the top level can reach live in a by-name global environment.

The dispatch loop's frame stack is a *Rust local* rather than VM state, so a native builtin like `map` or `filter` re-enters the VM by running a fresh frame stack to completion, as ordinary Rust recursion over the shared globals and stdout.

### Where a top-level binding lives

A global slot has to be *read into* a register before anything can use it, and written back after. A top-level `total = total + i` therefore costs a load and a store where the same line inside a `fn` costs neither, because a function-local's register *is* the binding.

The compiler puts a top-level binding in the entry frame's registers whenever nothing outside the top level can reach it by name, so a script's loop costs what the same loop costs inside a function.

A binding stays in the global table exactly when something could look it up there. These are the ways something can:

| What reaches a top-level binding | How |
|---|---|
| A named `fn`'s `use (…)` list | A named `fn` is sealed, so `use (…)` is its only route to the top level. |
| A closure, a field default, a method's `use (…)` list | Each reaches one implicitly. |
| An `isolate` worker | Is shipped the globals by slot. |
| A free-function [`invoke(name, args)`](Attributes-and-Reflection) | Resolves its name against the top level at run time, so it could name any binding at all. |
| A `destruct` declaration in the program | End-of-program destruction runs every top-level binding's destructor in reverse declaration order, and the global table's teardown is what performs it. |

The interactive tools always use the global table. `noeta repl`, the debug console and `noeta serve`'s hot reload each compile a *second* program into the running one, replacing the entry chunk, and a global slot persists across that where a retired chunk's registers do not.

So a binding you declare at the prompt is still there in the next entry, and a hot-swapped edit re-runs against the state the previous version left.

A register-held binding is a named local of the entry frame, so `noeta dap` shows it in the same Variables view, under the same name, at the same frame as one held in a slot.

### Register allocation

IR→bytecode first allocates registers *monotonically* (every temporary and local gets a fresh slot living to teardown); a bytecode→bytecode post-pass (`noeta-compiler`'s `regalloc::coalesce`) then reclaims the waste by graph coloring:

1. **Liveness**, a backward dataflow to a fixpoint over the real control-flow graph.
2. **Interference graph**. A definition interferes with everything in its `live_out`, plus a def↔use edge forbidding one op's source and destination sharing a slot.
3. **Greedy coloring**. Parameters are pre-colored to `0..num_params` (the calling convention addresses them by index); `num_registers` shrinks to the color count.

Coalescing buys smaller per-activation register arrays and *prompt* reclamation. When a later value reuses a slot, the write releases the previous dead occupant right there rather than at teardown. Destructor-bearing locals are pinned so a panic-teardown drop keeps its value through coalescing.

## NaN-boxing

Every runtime value is a single 64-bit word, `struct Value(u64)` in `noeta-value`. Doubles are stored natively. Everything else is encoded in the payload of a quiet NaN with a small type tag, and heap pointers live in the low 48 bits:

```text
float      : any bits where (bits & QNAN) != QNAN     (a real double)
pointer    : SIGN_BIT | QNAN | addr48                 (a refcounted heap object)
small int  : QNAN | INT_TAG | payload48 (sign-ext)    (immediate, ±2^47)
f32        : QNAN | F32_TAG | bits32                   (immediate packed f32)
unit/bool  : QNAN | TAG_{UNIT,FALSE,TRUE}             (immediate singletons)
pending    : QNAN | TAG_PENDING                        (the async-suspend sentinel)
```

NaN-boxing keeps every value pointer-sized and cache-friendly, which is what the hot dispatch path wants (the LuaJIT and JavaScriptCore approach). An `i64` too large for the 48-bit immediate is boxed on the heap as `Payload::Int`, so full-width integers work.

A heap pointer refers to an `Obj`, a `#[repr(C)]` header plus a `Payload`. The payload variants cover everything heap-allocated: `Str`, `Bytes`, boxed `Int`, `Closure { proto, upvalues }`, `Cell` (a mutable upvalue cell), `List`, `Tuple`, `Set`, `Map`, `PackedList { schema, bytes }` (a flat unboxed numeric buffer), shaped `Object`/`Enum`, `Extern` (the generic registered-extension leaf that file handles, UUIDs and other native types share), channel `Sender`/`Receiver`, and iterator state.

The header carries the `refcount`, a monotonic creation `seq` (for deterministic destruction tie-breaks), the cycle collector's `color`/`buffered` flags, and the isolates `shared` bit.

> [!NOTE]
> **`noeta-value` is the one crate whose core value model uses `unsafe`**, for the NaN-box pointer round-trip and for heap-header access. It opts out of the workspace's `unsafe_code = "forbid"` lint and is miri-gated. It also emits the refcount primitives and the operator semantics (`apply_binary`, `structural_compare`, …) that *both* backends share, so structural equality and ordering agree by construction.
>
> The compiler crate is itself `unsafe`-free. The VM crate sets `unsafe_code = "deny"` rather than the workspace's `forbid`, with a few explicitly `#[allow]`ed sites at the Tier-1 JIT's native-ABI boundary: reconstituting VM state from the Cranelift-supplied pointers, and the fast call convention's register-stack reservation. Everything else in both crates stays `unsafe`-free.

## Shapes (hidden classes)

Each aggregate points to a **shape**, a V8/JSC-style hidden class describing its layout, and the fields live in a flat inline slot array indexed by that shape. This is `noeta-object`, pure layout data below `noeta-value` in the DAG.

- A `Shape` records the type name, the ordered slot names, and enum-variant info. `ShapeKind` is `Struct`, `Class`, `Opaque` or `Enum`, and `slot_of(name)` resolves a field name to a slot index.
- The compiler emits a flat shape table into the `Module`. The VM wraps each entry in an `Rc<Shape>` once at startup and clones that handle into every value of that shape, so **shape identity is a cheap pointer comparison**, which is what the inline cache keys on.
- Field access reads the `Rc<Shape>`, resolves `slot_of(name)` to an index, and indexes the flat slot array. A structural update (`Money { amount: 300, ..a }`) copies slots shallowly under the same shape.

A shared shape also carries per-instance type identity cheaply, which is what **generics** rest on: a `Box<User>` is a shape carrying a type-parameter slot.

## Inline caches

Every property-access and method-call site caches the last shape it saw and the resolved slot/prototype, so a repeated monomorphic access skips the lookup.

- The compiler assigns each `LoadField`/`CallMethod` op a cache slot id via a module-global counter, giving `Module.cache_slots`.
- The VM allocates a per-run side array `Vec<Option<(&'static Shape, u32)>>` sized to `cache_slots`. It is a local in the dispatch loop, so it holds no borrow of `self` across the loop and does not persist between runs.
- **Hit**: the receiver's shape pointer equals the cached one, and the cached slot or prototype is used directly. That is a raw pointer compare, with no refcount bump.
- **Miss**: the VM resolves via `slot_of` for a field or the `(type, method)` hashmap for a method, then refreshes the entry. The cached key is an interned `&'static Shape`, so it always points at a live shape.

`LoadField` caches the field slot index, skipping the name scan. `CallMethod` caches the method prototype, skipping the hashmap lookup and its two string clones, which are the dominant cost.

Shapes are immutable once created, so a cached `(shape, slot)` stays correct for as long as the entry lives, and a receiver with a different shape misses and refreshes. The cache is VM-only and invisible to `RunResult`, so the differential is unaffected.

## Tier 1 — the JIT

Everything above is **Tier 0**, the interpreter dispatch loop. On top of it sits an optional **Tier 1**, a method-at-a-time [Cranelift](https://cranelift.dev/) JIT (`noeta-jit`) that compiles a hot prototype to native machine code.

Tier 1 lives behind a `jit` cargo feature. The default `noeta` binary enables it, and a `--no-default-features` build pulls in *zero* Cranelift crates and runs Tier 0 only, byte-for-byte identically.

The sandbox, the conformance corpus, and isolate worker threads all run Tier 0, because the JIT's `JITModule` is `!Send`. The differential oracle's baseline is therefore always interpreted code.

### The design in one line

**The fast path is native, and everything else calls back into the interpreter's own code.** Integer and float arithmetic, comparisons, branches, `LoadConst`/`Move`, global-slot access, and calls compile to real Cranelift IR. Anything richer, such as a heap or collection op or an uncompiled callee, becomes a call to a *runtime helper* that runs the interpreter's exact arm, refcounts included.

Tier 1 therefore agrees with Tier 0 by construction. What it specializes is guarded, and what it does not specialize it delegates.

### The shared stack makes deopt free

A compiled prototype runs on the **same contiguous register stack** the interpreter uses, with the ABI `fn(vm, regs, base, globals, frames, regs_vec, entry_pc) -> i64`. Native code reads and writes the interpreter's real registers, so **deoptimization costs nothing to set up**.

When native code reaches a `Return`, or a guard fails, it *returns the bytecode pc of that op* and the interpreter resumes there over the already-correct register window. A guard fails when an operand turns out to be something other than a small int, when an add overflows the 48-bit immediate range, or when an `if` condition is not a bool.

Guards always **bail before mutating any state**, so the interpreter re-runs the op cleanly. That contract is what lets an untyped bytecode be compiled speculatively without a separate deopt-state map.

### Registers live in SSA (mem2reg)

Within a compiled prototype, VM registers are **Cranelift SSA variables**, not memory:

- **Registers live in machine registers for the whole native region**, heap values included. The in-memory register stack is touched at region boundaries only: entry loads, bail-edge spills, and helper syncs.
- **Typed values run unboxed.** A forward kind dataflow proves registers `Int`, `Bool` or `Float` along native paths, and a second *raw* (unboxed) variable per register lets typed arithmetic skip the NaN-box tag checks and the box/unbox chains. A counting loop's governing compare compiles to one `cmp; jl`.
- **Claims are verified at entries.** Every mid-frame entry, meaning a resume after a call or an OSR loop header, checks the claimed registers against what Tier 0 actually left in the slots and bails on a violation. Tier 0 can heap-box an overflow exactly where the native path would have bailed first, and a wrongly-trusted claim would corrupt refcounts.

### Getting hot, and getting in mid-loop (OSR)

Promotion is a per-prototype counter. A prototype crossing `JIT_HOT_THRESHOLD` frame entries **or loop back-edges** is compiled.

The back-edge trigger is **on-stack replacement (OSR)**. A long-running loop enters Tier 1 *at its loop header*, mid-frame, rather than waiting for the next call, so loop headers become native re-entry points and reuse the same mid-frame-entry machinery that re-enters a compiled caller after a call returns. That is what gets a top-level program that is one big loop into Tier 1, since its `main` frame is entered exactly once.

**How a prototype got hot decides what gets compiled.** One that got hot at a *call* is compiled whole. One that got hot at a *back-edge* is compiled to its **OSR window**: the loop's own extent, grown to cover every enclosing loop, with everything outside it left to the interpreter.

The window keeps the loop's register pressure its own. Cranelift allocates registers over a whole function, so compiling a script's cold prologue and tail alongside its hot loop puts them in competition for machine registers, and a value the loop wanted in a register spills.

Both bodies coexist, and a native re-entry is routed by whether its pc falls in a window, so a windowed prototype keeps full coverage.

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

A native `Call` first consults a **per-call-site inline cache**. It hits when the callee closure is the one seen last time, which bits-equality proves because the cached closure is pinned, and the entire call setup is then native:

- **Zero helper calls on the hot path.** Native code capacity-checks the stacks, extends the register stack over an **uninitialized** callee window, writes the frame record from a baked template, and calls the callee's **fast-convention body**. Arguments travel as machine arguments and the result comes back as a return value, and the return protocol (masked slot releases, frame pop, stack truncation) is emitted inline from the baked frame layout. Recursive `fib` runs frame-to-frame in native code.
- **Every native exit normalizes the window.** Native code spills the live and heap-desynced registers and unit-fills the rest before the interpreter, or an abort's unwind, can look. The soundness contract is that whatever looks at the window sees an initialized one.
- **The frame stack stays fully honest.** A cache miss or an un-direct-able callee falls back to a helper that pushes the callee frame for the interpreter. Either way every call pushes a real frame, which is what keeps deopt and unwinding trivial.

### Refcounts across the tier boundary

A prototype that keeps a heap value in a register is compiled **heap-aware**. Overwriting a possibly-heap register releases the old value straight from its SSA variable, with no load, and moving one retains it, which matches the interpreter's `set_reg`. Where a dataflow proves a value immediate, that refcount work is elided.

A heap value's slot can be out of sync with its variable, holding a released pointer or missing the reference the variable owns. A *slot-hazard* dataflow therefore extends every sync point's spill set, so teardown and unwinding always see an ownership-exact window.

### The oracle

Tier 1 has its own gate, separate from the eval↔VM differential. **`--jit-differential`** runs every corpus program through the interpreter and through the forced-Tier-1 JIT, and asserts four things at once:

| Assertion | What it holds |
|---|---|
| **A byte-identical `RunResult`** | The JIT's observable behavior equals the interpreter's. |
| **Zero heap residency** | Every program frees everything it allocated under native code. |
| **Zero refcount anomalies** | During cycle collection, every unreachable object's refcount equals its in-edges from the garbage set. |
| **No skipped destructors** | Every object whose type declares `destruct` ran its destructor. |

The anomaly check covers what residency alone misses. Unreachable garbage can only reference itself, so a *skipped* retain or release shows as a refcount out of step with the in-edges even when teardown's backup sweep would have absorbed the orphan, which is the failure mode of a wrong immediacy claim.

The destructor check covers the one memory bug that produces a *wrong answer* rather than a residual. Residency cannot see an object freed with its `destruct` skipped, and neither can the output check when the interpreter skips it too. Objects allocated with a destructor-bearing shape are weighed against destructors run, and zero residency in the same measurement means everything allocated was freed, so a surplus allocation is exactly such an object.

The oracle has a **second arm**, `--jit-differential --cancel-poll`, which runs the same corpus with a never-set cancellation flag armed on the JIT side. A run that can be cancelled gets different native code, carrying a cancellation poll at every loop header (see [Concurrency Internals](Concurrency-Internals#the-third-safepoint-a-jit-loop-header)), so the two shapes are gated separately.

Refcount exactness is the thing most likely to drift when native code manages the heap, and JIT-generated code cannot run under miri, so these checks are as load-bearing as the output check.

> [!NOTE]
> **Where Tier 1 pays.** Its wins are in native arithmetic, control flow, calls, and OSR'd loops. A field-bearing loop is bounded by the dependent-load latency of the read and by the heap-aware store discipline, both of which are tier-independent, so the interpreter's own inline cache is what serves it.

## See also

- [Memory Management](Memory-Management) — the refcount discipline the VM's registers and heap follow, and what the JIT reproduces across the tier boundary.
- [Performance Techniques](Performance-Techniques) — inline caches, layout, the JIT, and what was measured.
