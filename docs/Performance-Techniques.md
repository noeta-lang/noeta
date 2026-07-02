# Performance Techniques

A round-up of the performance story, including the numeric-layout work and one instructive negative result. Several techniques have their own pages; this page ties them together and covers SIMD/layout in depth.

## The techniques at a glance

| Technique | Where | Page |
|---|---|---|
| NaN-boxed values (pointer-sized, cache-friendly) | `lang-value` | [The Virtual Machine](The-Virtual-Machine) |
| Shapes / hidden classes (flat-slot layout) | `lang-object` | [The Virtual Machine](The-Virtual-Machine) |
| Inline caches on property/method sites | `lang-vm` | [The Virtual Machine](The-Virtual-Machine) |
| Register allocation via graph coloring | `lang-compiler` | [The Virtual Machine](The-Virtual-Machine) |
| Compiled precise reference counting | `lang-ir-passes` | [Memory Management](Memory-Management) |
| In-place reuse (O(n²) → O(n) appends) | `lang-ir-passes` | [Memory Management](Memory-Management) |
| Incremental recompilation (salsa) | `lang-db` | [Architecture & Pipeline](Architecture-and-Pipeline) |
| Numeric layout + autovectorization | `lang-stdlib` | this page |

## Memory management is a performance feature

The single biggest performance decision is that memory management is *compiled*, not traced: reference counts are inserted at compile time, values are freed at their last use, and unique-owner mutations become in-place updates. There is no stop-the-world pause on the hot path, and the classic `acc ~= [x]`-in-a-loop quadratic blowup is compiled away into an in-place extension. See [Memory Management](Memory-Management).

## Dispatch is optimized by layout, not tricks

The VM's speed comes from *cheap, predictable* operations: values are one word (NaN-boxing), objects are flat slot arrays keyed by a shared shape (no per-instance hashmap), shape identity is a pointer compare, and every property/method site caches its last shape (inline caches, ~−22% on member dispatch). See [The Virtual Machine](The-Virtual-Machine).

## Numeric layout and SIMD — a case study

NaN-boxing and shapes optimize *dispatch*, but do nothing for *data layout* — and SIMD/numeric throughput is a **layout** problem. A 64-bit boxed value with a tag is the wrong representation for packed numerics: 10,000 `Vec3`s stored as shaped heap objects are cache-hostile and un-vectorizable.

So the type system distinguishes flexible dynamic objects (the default) from **packed value types** (flat, unboxed, cache-friendly). A `List<Vec3>` is stored as a flat `PackedList { schema, bytes }` buffer rather than an array of object pointers, and the bulk kernels stream over the raw `&[u8]` byte-direct (`f32::from_le_bytes`/`to_le_bytes`, which fold to plain little-endian loads/stores). These kernels stay per-backend so both call the *same* Rust code and the differential pins the result.

### The instructive negative result

**Explicit SIMD was measured slower and dropped.** The `wide` crate (`f32x8`) was tried twice — on the array-of-structs buffer and again on struct-of-arrays columns — and regressed **1.8×–9×** both times. Two reasons:

1. The array-of-structs `chunks_exact` loop is *already* autovectorized by LLVM, so hand-SIMD only got in the way.
2. The 1-byte-aligned array-of-structs buffer forces a scalar gather/scatter to fill lane registers, which dominates.

The shipped win instead comes from a **struct-of-arrays layout that unlocks LLVM autovectorization**: an opt-in `SoaVec3` columnar type (three contiguous `f32` columns), built once from a `List<Vec3>` via an O(n) transpose and reduced many times, giving **2.7×–4×** on `dot`/`length`. The surface is the `vec.soa*` family (`soa`, `soa_dot`, `soa_length`, `soa_add`, `soa_scale`, …).

> [!NOTE]
> The column→`&[f32]` reinterpret in the shipped path is done via the safe, checked `bytemuck` crate (falling back to the byte-read path when a buffer is misaligned), so `lang-stdlib` stays `unsafe`-free there. The lesson: *the right layout plus the compiler's autovectorizer beat hand-written intrinsics here* — measure before reaching for SIMD.

## Incremental compilation

For editor-speed feedback, the compiler is a salsa query graph: editing one module (or one function) recomputes only its transitive dependents. This is the same machinery a future LSP and hot-module-reload would use. See [Architecture & Pipeline](Architecture-and-Pipeline#incremental-compilation-salsa).

## What is designed but deferred

Honestly noted, so the picture is complete:

- **Zero-copy cross-thread borrow-share** for isolates (built and miri-proven, unwired — blocked on `Rc<Shape> → Arc<Shape>`). See [Concurrency Internals](Concurrency-Internals).
- **A Tier-1 self-specializing interpreter and a copy-and-patch JIT** — the IR is "JIT-ready" (a third consumer), but the JIT is a later milestone.
- **A `gc-arena` tracing path** for destructor-free classes.
- **Explicit SIMD intrinsics** (`Simd<T, N>` + const generics) as a later track — the current approach deliberately relies on layout + autovectorization instead.

## See also

- [The Virtual Machine](The-Virtual-Machine) · [Memory Management](Memory-Management) — the two biggest performance stories, in depth.
- [Fixed-Width Integers](Fixed-Width-Integers) — the packed value types the numeric layout builds on.
