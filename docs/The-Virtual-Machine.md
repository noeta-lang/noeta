# The Virtual Machine

How the bytecode backend executes a program: a register machine over NaN-boxed values, a shape-based object model, and inline caches on every property access.

## A register-based bytecode VM

The VM (`lang-vm`) is a Tier-0 **register machine** (Lua/Dalvik style), not a stack machine. Register bytecode issues fewer dispatches per operation than a stack VM and is a friendlier base for a later specializing interpreter or JIT.

The compiled artifact (pure data in `lang-bytecode`):

- **`Op`** — the opcode set: arithmetic and branching, `Call`/`Return`, `MakeList`/`MakeMap` and iteration ops, `CallBuiltin`, the object-model ops (`MakeRecord`/`MakeEnum`/`LoadField`/`CallMethod`), and ops added by later tracks (`MakeStructInPlace` for reuse, drop ops for RC, `MakeChannel`/`SpawnIsolate`/`ExtCall`).
- **`Chunk`** — one function prototype: its `code`, constant pool, `num_params`, `num_registers`, and `frame_locals` (destructor-teardown pins). `Chunk::disassemble` gives stable text for snapshot tests.
- **`Module`** — the prototype table (proto 0 is the top-level program; the rest are functions/closures/methods), the flat `shapes` layout table, the `methods` dispatch table, and `cache_slots` (the inline-cache count).

Execution is frame-based: each call pushes a `Frame` with its own register file, program counter, and return slot. Register 0 of a method frame is the receiver. Top-level bindings and function names live in a by-name global environment. Notably, the dispatch loop's frame stack is a *Rust local*, not VM state — so a native builtin like `map`/`filter` re-enters the VM by running a fresh frame stack to completion (ordinary Rust recursion over the shared globals/stdout).

### Register allocation

IR→bytecode first allocates registers *monotonically* (every temporary and local gets a fresh slot living to teardown); a bytecode→bytecode post-pass (`lang-compiler`'s `regalloc::coalesce`) then reclaims the waste by graph coloring:

1. **Liveness** — a backward dataflow to a fixpoint over the real control-flow graph.
2. **Interference graph** — a definition interferes with everything in its `live_out`, plus a def↔use edge forbidding one op's source and destination sharing a slot.
3. **Greedy coloring** — parameters pre-colored to `0..num_params` (the calling convention addresses them by index); `num_registers` shrinks to the color count.

The payoff is both smaller per-activation register arrays and *prompt* reclamation: when a later value reuses a slot, the write releases the previous dead occupant right there instead of at teardown. Destructor-bearing locals are pinned so a panic-teardown drop never loses its value to coalescing.

## NaN-boxing

Every runtime value is a single 64-bit word — `struct Value(u64)` in `lang-value`. Doubles are stored natively; everything else is encoded in the payload of a quiet NaN with a small type tag, and heap pointers live in the low 48 bits:

```
float      : any bits where (bits & QNAN) != QNAN     (a real double)
pointer    : SIGN_BIT | QNAN | addr48                 (a refcounted heap object)
small int  : QNAN | INT_TAG | payload48 (sign-ext)    (immediate, ±2^47)
f32        : QNAN | F32_TAG | bits32                   (immediate packed f32)
unit/bool  : QNAN | TAG_{UNIT,FALSE,TRUE}             (immediate singletons)
pending    : QNAN | TAG_PENDING                        (the async-suspend sentinel)
```

Why: a fat `enum Value` would be 16+ bytes and branchy; NaN-boxing keeps values pointer-sized and cache-friendly (the LuaJIT / JavaScriptCore approach), which matters on the hot dispatch path. An `i64` too large for the 48-bit immediate is boxed on the heap as `Payload::Int`, so full-width integers still work.

A heap pointer refers to an `Obj` = a `#[repr(C)]` header + a `Payload`. The payload variants cover everything heap-allocated: `Str`, `Bytes`, boxed `Int`, `Closure { proto, upvalues }`, `Cell` (a mutable upvalue cell), `List`, `Tuple`, `Set`, `Map`, `PackedList { schema, bytes }` (a flat unboxed numeric buffer), shaped `Object`/`Enum`, `FileHandle`, channel `Sender`/`Receiver`, and iterator state. The header carries the `refcount`, a monotonic creation `seq` (for deterministic destruction tie-breaks), the cycle collector's `color`/`buffered` flags, and the isolates `shared` bit.

> [!NOTE]
> **`lang-value` is the one crate whose core value model uses `unsafe`** — the NaN-box pointer round-trip and heap-header access. It opts out of the workspace's `unsafe_code = "forbid"` lint and is miri-gated. It also emits the refcount primitives and the operator semantics (`apply_binary`, `structural_compare`, …) that *both* backends share, so structural equality and ordering cannot diverge. The VM and compiler crates are themselves `unsafe`-free.

## Shapes (hidden classes)

Objects are not per-instance hashmaps. Each aggregate points to a **shape** (a V8/JSC-style hidden class) describing its layout; the fields live in a flat inline slot array indexed by the shape. This is `lang-object`, pure layout data below `lang-value` in the DAG.

- A `Shape` records the type name, the ordered slot names, and enum-variant info; `ShapeKind` is `Struct`/`Class`/`Opaque`/`Enum`; `slot_of(name)` resolves a field name to a slot index.
- The compiler emits a flat shape table into the `Module`; the VM wraps each entry in an `Rc<Shape>` once at startup and clones that handle into every value of that shape — so **shape identity is a cheap pointer comparison**, which is exactly what the inline cache keys on.
- Field access is: read the `Rc<Shape>`, `slot_of(name)` → index, index the flat slot array. A structural update (`Money { amount: 300, ..a }`) copies slots shallowly under the same shape.

Beyond speed, a shared shape gives per-instance type identity cheaply — which is how **generics fall out**: a `Box<User>` is a shape carrying a type-parameter slot, the per-instance type identity a dynamic language usually lacks.

## Inline caches

Every property-access and method-call site caches the last shape it saw and the resolved slot/prototype, so a repeated monomorphic access skips the lookup. (This *is* shipped — some older crate READMEs calling it "deferred" are stale.)

- The compiler assigns each `LoadField`/`CallMethod` op a cache slot id via a module-global counter (→ `Module.cache_slots`).
- The VM allocates a per-run side array `Vec<Option<(Rc<Shape>, u32)>>` sized to `cache_slots` — a local in the dispatch loop, so it neither borrows `self` across the loop nor leaks between runs.
- **Hit**: the receiver's shape pointer equals the cached one → use the cached slot/prototype directly (a raw pointer compare, no refcount bump).
- **Miss**: resolve via `slot_of` (field) or the `(type, method)` hashmap (method), then refresh the entry — storing an `Rc<Shape>` *clone* so the cached key can never alias a freed shape.

`LoadField` caches the field slot index (skipping the name scan); `CallMethod` caches the method prototype (skipping the hashmap lookup *and its two string clones*, the dominant cost). Shapes are immutable once created, so a cached `(shape, slot)` never goes stale — a different shape simply misses and refreshes. The cache is VM-only and invisible to `RunResult`, so the differential is unaffected; measured impact is roughly −22% on member dispatch.

## See also

- [Memory Management](Memory-Management) — the refcount discipline the VM's registers and heap follow.
- [Performance Techniques](Performance-Techniques) — inline caches, layout, and what was measured.
