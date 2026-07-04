# Performance Techniques

A round-up of the performance story, including the numeric-layout work and one instructive negative result. Several techniques have their own pages; this page ties them together and covers SIMD/layout in depth.

## The techniques at a glance

| Technique | Where | Page |
|---|---|---|
| NaN-boxed values (pointer-sized, cache-friendly) | `noeta-value` | [The Virtual Machine](The-Virtual-Machine) |
| Shapes / hidden classes (flat-slot layout) | `noeta-object` | [The Virtual Machine](The-Virtual-Machine) |
| Inline caches on property/method sites | `noeta-vm` | [The Virtual Machine](The-Virtual-Machine) |
| Register allocation via graph coloring | `noeta-compiler` | [The Virtual Machine](The-Virtual-Machine) |
| Tier-1 Cranelift JIT (hot-counter + OSR) | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#tier-1--the-jit) |
| Compiled precise reference counting | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| In-place reuse (O(n²) → O(n) appends) | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| Incremental recompilation (salsa) | `noeta-db` | [Architecture & Pipeline](Architecture-and-Pipeline) |
| Numeric layout + autovectorization | `noeta-stdlib` | this page |

## Memory management is a performance feature

The single biggest performance decision is that memory management is *compiled*, not traced: reference counts are inserted at compile time, values are freed at their last use, and unique-owner mutations become in-place updates. There is no stop-the-world pause on the hot path, and the classic `acc ~= [x]`-in-a-loop quadratic blowup is compiled away into an in-place extension. See [Memory Management](Memory-Management).

## Dispatch is optimized by layout, not tricks

The VM's speed comes from *cheap, predictable* operations: values are one word (NaN-boxing), objects are flat slot arrays keyed by a shared shape (no per-instance hashmap), shape identity is a pointer compare, and every property/method site caches its last shape (inline caches, ~−22% on member dispatch). See [The Virtual Machine](The-Virtual-Machine).

## A second tier: the JIT

The interpreter above is Tier 0. Hot prototypes are compiled to native code by a **Tier-1 [Cranelift](https://cranelift.dev/) JIT** (`noeta-jit`, behind the `jit` cargo feature — on in the shipped binary, absent in a `--no-default-features` build). It is a *method* JIT: integer/float arithmetic, comparisons, branches, and slot access compile to native IR with the NaN-box guards inlined; calls and heap ops call back into the interpreter's own code, so the two tiers can never disagree. Promotion is a hot-counter plus **on-stack replacement (OSR)** so a long-running loop can go native mid-frame, and compiled code runs on the interpreter's own register stack so deopt is a bare pc-return. A dedicated `--jit-differential` oracle asserts the JIT is byte-identical to the interpreter *and* leak-free on every program. Measured speedups: ~6–8× on numeric loops, ~2.3× on recursive calls, ~3.5–5.5× on OSR'd top-level loops; ~19–28% of that came from the bare-store refinement alone. Full mechanism (deopt contract, native calls, refcounts across the tier boundary, and one instructive negative result) in [The Virtual Machine → Tier 1](The-Virtual-Machine#tier-1--the-jit).

## Numeric layout and SIMD — a case study

NaN-boxing and shapes optimize *dispatch*, but do nothing for *data layout* — and SIMD/numeric throughput is a **layout** problem. A 64-bit boxed value with a tag is the wrong representation for packed numerics: 10,000 `Vec3`s stored as shaped heap objects are cache-hostile and un-vectorizable.

So the type system distinguishes flexible dynamic objects (the default) from **packed value types** (flat, unboxed, cache-friendly). A `List<Vec3>` is stored as a flat `PackedList { schema, bytes }` buffer rather than an array of object pointers, and the bulk kernels stream over the raw `&[u8]` byte-direct (`f32::from_le_bytes`/`to_le_bytes`, which fold to plain little-endian loads/stores). These kernels stay per-backend so both call the *same* Rust code and the differential pins the result.

### The instructive negative result

**Explicit SIMD was measured slower and dropped.** The `wide` crate (`f32x8`) was tried twice — on the array-of-structs buffer and again on struct-of-arrays columns — and regressed **1.8×–9×** both times. Two reasons:

1. The array-of-structs `chunks_exact` loop is *already* autovectorized by LLVM, so hand-SIMD only got in the way.
2. The 1-byte-aligned array-of-structs buffer forces a scalar gather/scatter to fill lane registers, which dominates.

The shipped win instead comes from a **struct-of-arrays layout that unlocks LLVM autovectorization**: an opt-in `SoaVec3` columnar type (three contiguous `f32` columns), built once from a `List<Vec3>` via an O(n) transpose and reduced many times, giving **2.7×–4×** on `dot`/`length`. The surface is the `vec.soa*` family (`soa`, `soa_dot`, `soa_length`, `soa_add`, `soa_scale`, …).

> [!NOTE]
> The column→`&[f32]` reinterpret in the shipped path is done via the safe, checked `bytemuck` crate (falling back to the byte-read path when a buffer is misaligned), so `noeta-stdlib` stays `unsafe`-free there. The lesson: *the right layout plus the compiler's autovectorizer beat hand-written intrinsics here* — measure before reaching for SIMD.

## Incremental compilation

For editor-speed feedback, the compiler is a salsa query graph: editing one module (or one function) recomputes only its transitive dependents. This is the same machinery a future LSP and hot-module-reload would use. See [Architecture & Pipeline](Architecture-and-Pipeline#incremental-compilation-salsa).

## What is designed but deferred

Honestly noted, so the picture is complete:

- **Zero-copy cross-thread borrow-share** for isolates (built and miri-proven, unwired — blocked on `Rc<Shape> → Arc<Shape>`). See [Concurrency Internals](Concurrency-Internals).
- **A `gc-arena` tracing path** for destructor-free classes.
- **Explicit SIMD intrinsics** (`Simd<T, N>` + const generics) as a later track — the current approach deliberately relies on layout + autovectorization instead.

## See also

- [The Virtual Machine](The-Virtual-Machine) · [Memory Management](Memory-Management) — the two biggest performance stories, in depth.
- [Fixed-Width Integers](Fixed-Width-Integers) — the packed value types the numeric layout builds on.
