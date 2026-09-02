# Performance Techniques

Where Noeta's speed comes from. Several techniques have their own pages; this one ties them together and covers numeric layout in depth.

## The techniques at a glance

| Technique | Where | Page |
|---|---|---|
| NaN-boxed values (pointer-sized, cache-friendly) | `noeta-value` | [The Virtual Machine](The-Virtual-Machine) |
| Shapes / hidden classes (flat-slot layout) | `noeta-object` | [The Virtual Machine](The-Virtual-Machine) |
| Inline caches on property/method sites | `noeta-vm` | [The Virtual Machine](The-Virtual-Machine) |
| Register allocation via graph coloring | `noeta-compiler` | [The Virtual Machine](The-Virtual-Machine) |
| Top-level bindings held in the entry frame's registers | `noeta-compiler` | [The Virtual Machine](The-Virtual-Machine#where-a-top-level-binding-lives) |
| Tier-1 Cranelift JIT (hot-counter + OSR) | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#tier-1--the-jit) |
| SSA register promotion + typed/unboxed values | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#registers-live-in-ssa-mem2reg) |
| Fast call convention + call-site inline caches | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#calls-stay-native--the-fast-call-convention) |
| Compiled precise reference counting | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| In-place reuse (O(n²) → O(n) appends) | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| Incremental recompilation (salsa) | `noeta-db` | [Architecture & Pipeline](Architecture-and-Pipeline) |
| Numeric layout + autovectorization | `noeta-stdlib` | this page |

## Finding the hot spots

Measure before optimizing. [`noeta profile`](Profiling) reports where a program spends its time, as an exact per-function call-count and self-time table (`--instrument`) or a wall-time **flamegraph** (SVG or speedscope). It profiles the production VM's tier 0, so it shows the language-level shape of a run, meaning which function and line is hot. See [Profiling](Profiling) for what tier 0 does and does not reflect.

## Memory management is a performance feature

The largest performance decision is that memory management is *compiled* rather than traced. Reference counts are inserted at compile time, values are freed at their last use, and unique-owner mutations become in-place updates.

There is no stop-the-world pause on the hot path, and the classic `acc ~= [x]`-in-a-loop quadratic blowup compiles away into an in-place extension. See [Memory Management](Memory-Management).

## A script pays what a function pays

Noeta programs are written at the top level, so the top level has to be as fast as a function body. A top-level binding lives in the entry frame's registers whenever nothing outside the top level can reach it by name, and a loop written at the top level compiles to the same instructions as the identical loop inside a `fn`, with no per-iteration load and store through a by-name table.

[The Virtual Machine → Where a top-level binding lives](The-Virtual-Machine#where-a-top-level-binding-lives) states the exact rule, including the cases that keep a binding in the global table: a `use (…)` capture, a closure, a by-name `invoke`, a `destruct`, and an interactive session.

## Dispatch is optimized by layout

The VM's speed comes from cheap, predictable operations. Values are one word (NaN-boxing), objects are flat slot arrays keyed by a shared shape rather than a per-instance hashmap, shape identity is a pointer compare, and every property and method site caches its last shape in an inline cache. See [The Virtual Machine](The-Virtual-Machine).

## A second tier: the JIT

The interpreter above is Tier 0. Hot prototypes, found by a hot counter plus on-stack replacement, are compiled to native code by a **Tier-1 [Cranelift](https://cranelift.dev/) JIT** (`noeta-jit`, behind the `jit` cargo feature: on in the shipped binary, absent in a `--no-default-features` build).

It is a *method* JIT with the values genuinely in machine registers: SSA across the compiled region, typed arithmetic unboxed, and a fast native call convention. Anything it does not specialize calls back into the interpreter's own code, so the two tiers cannot disagree.

A dedicated `--jit-differential` oracle asserts the JIT is byte-identical to the interpreter, leak-free, and refcount-anomaly-free on every program in the corpus. The full mechanism, covering the deopt contract, verified claims, the fast call convention, and refcounts across the tier boundary, is in [The Virtual Machine → Tier 1](The-Virtual-Machine#tier-1--the-jit).

## Numeric layout and SIMD

NaN-boxing and shapes optimize *dispatch* and do nothing for *data layout*, and numeric throughput is a layout problem. A 64-bit boxed value with a tag is the wrong representation for packed numerics: 10,000 `Vec3`s stored as shaped heap objects are cache-hostile and un-vectorizable.

So the type system distinguishes flexible dynamic objects, the default, from **packed value types**, which are flat, unboxed and cache-friendly. A `List<Vec3>` is stored as a flat `PackedList { schema, bytes }` buffer rather than an array of object pointers, and the bulk kernels stream over the raw `&[u8]` byte-direct, through `f32::from_le_bytes`/`to_le_bytes`, which fold to plain little-endian loads and stores. These kernels stay per-backend so both call the same Rust code and the differential pins the result.

### Columnar layout

`@packed(Layout.Column)` stores a struct's fields as separate contiguous columns, a struct-of-arrays layout, and that is what unlocks LLVM's autovectorizer on the bulk kernels:

```noeta
use std.{vec}

@packed(Layout.Column) struct V3 { x: f32; y: f32; z: f32 }

ps = [V3 { x: 3.0f32, y: 4.0f32, z: 0.0f32 }, V3 { x: 1.0f32, y: 2.0f32, z: 2.0f32 }]
qs = [V3 { x: 1.0f32, y: 0.0f32, z: 0.0f32 }, V3 { x: 0.0f32, y: 3.0f32, z: 4.0f32 }]

echo vec.length_all(ps)        // reductions run the columnar kernel
echo vec.dot_all(ps, qs)
echo vec.add_all(ps, qs)       // producing kernels return a column list
```

A column buffer's bytes are already the columns, so `vec.length_all`, `vec.dot_all`, `vec.add_all`, `vec.sub_all` and `vec.scale_all` take the autovectorized path. Results stay bit-identical to the row layout: element-wise per lane, with reductions keeping the `(x·bx + y·by) + z·bz` order.

Explicit SIMD intrinsics through the `wide` crate were measured slower than this and are not used, for two reasons worth knowing before you reach for them yourself. The array-of-structs `chunks_exact` loop is already autovectorized by LLVM, so hand-written SIMD only gets in the way. And a 1-byte-aligned array-of-structs buffer forces a scalar gather and scatter to fill lane registers, which dominates the kernel. The right layout plus the compiler's autovectorizer is what wins here.

The layout is visible in the editor. Hovering a packed value or list shows the storage fact (`@packed — 12 bytes`; `flat packed storage — 12 bytes/element, column-major (SoA)`), and inlay type hints mark it compactly (`: List<Vec3> · flat`, `· SoA`). See [Editor and AI Tooling](Editor-and-AI-Tooling).

> [!NOTE]
> The column-to-`&[f32]` reinterpret is done through the safe, checked `bytemuck` crate, falling back to the byte-read path when a buffer is misaligned, so `noeta-stdlib` carries no `unsafe` of its own.

## Incremental compilation

For editor-speed feedback the compiler is a salsa query graph: editing one module, or one function, recomputes only its transitive dependents. The LSP, the MCP server and hot-module-reload all read from that graph. See [Architecture & Pipeline](Architecture-and-Pipeline#incremental-compilation-salsa).

## Not built

Explicit SIMD intrinsics (`Simd<T, N>` over const generics) are a roadmap item rather than a shipped feature. The columnar layout above, plus autovectorization, is the path today.

## See also

- [Profiling](Profiling) — find where time goes, with a hot-function table or a flamegraph.
- [The Virtual Machine](The-Virtual-Machine) · [Memory Management](Memory-Management) — the two biggest performance stories, in depth.
- [Fixed-Width Integers](Fixed-Width-Integers) — the packed value types the numeric layout builds on.
