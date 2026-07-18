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
| SSA register promotion + typed/unboxed values (P-JSSA) | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#registers-live-in-ssa-mem2reg) |
| Fast call convention + call-site inline caches | `noeta-jit` | [The Virtual Machine](The-Virtual-Machine#calls-stay-native--the-fast-call-convention) |
| Compiled precise reference counting | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| In-place reuse (O(n²) → O(n) appends) | `noeta-ir-passes` | [Memory Management](Memory-Management) |
| Incremental recompilation (salsa) | `noeta-db` | [Architecture & Pipeline](Architecture-and-Pipeline) |
| Numeric layout + autovectorization | `noeta-stdlib` | this page |

## Memory management is a performance feature

The single biggest performance decision is that memory management is *compiled*, not traced: reference counts are inserted at compile time, values are freed at their last use, and unique-owner mutations become in-place updates. There is no stop-the-world pause on the hot path, and the classic `acc ~= [x]`-in-a-loop quadratic blowup is compiled away into an in-place extension. See [Memory Management](Memory-Management).

## Dispatch is optimized by layout, not tricks

The VM's speed comes from *cheap, predictable* operations: values are one word (NaN-boxing), objects are flat slot arrays keyed by a shared shape (no per-instance hashmap), shape identity is a pointer compare, and every property/method site caches its last shape (inline caches, ~−22% on member dispatch). See [The Virtual Machine](The-Virtual-Machine).

## A second tier: the JIT

The interpreter above is Tier 0. Hot prototypes are compiled to native code by a **Tier-1 [Cranelift](https://cranelift.dev/) JIT** (`noeta-jit`, behind the `jit` cargo feature — on in the shipped binary, absent in a `--no-default-features` build). It is a *method* JIT with the values genuinely in machine registers: VM registers are held in **SSA** across the whole compiled region (heap values included), a kind dataflow lets typed arithmetic run **unboxed** with the NaN-box checks gone from loop bodies, and calls use a **fast convention** (per-call-site inline caches, arguments in machine arguments, native frame push and teardown) so recursion never leaves native code. Anything the JIT doesn't specialize calls back into the interpreter's own code, so the two tiers can never disagree; promotion is a hot-counter plus **on-stack replacement (OSR)**, and compiled code runs on the interpreter's own register stack so deopt is a bare pc-return. A dedicated `--jit-differential` oracle asserts the JIT is byte-identical to the interpreter, leak-free, *and* refcount-anomaly-free on every program. Measured on the cross-language suite: a fn-local counting loop runs ~2.3× faster and recursive `fib` ~4.3× faster than the pre-SSA JIT — putting Noeta in the method-JIT class on scalar loops (within ~1.7× of PHP 8.4's JIT) while keeping its existing wins (SoA columns, string building, startup). Full mechanism (deopt contract, verified claims, the fast call convention, refcounts across the tier boundary, and the instructive negative results) in [The Virtual Machine → Tier 1](The-Virtual-Machine#tier-1--the-jit); the milestone record with all measurements is `plans/jit/ssa.md`.

## Numeric layout and SIMD — a case study

NaN-boxing and shapes optimize *dispatch*, but do nothing for *data layout* — and SIMD/numeric throughput is a **layout** problem. A 64-bit boxed value with a tag is the wrong representation for packed numerics: 10,000 `Vec3`s stored as shaped heap objects are cache-hostile and un-vectorizable.

So the type system distinguishes flexible dynamic objects (the default) from **packed value types** (flat, unboxed, cache-friendly). A `List<Vec3>` is stored as a flat `PackedList { schema, bytes }` buffer rather than an array of object pointers, and the bulk kernels stream over the raw `&[u8]` byte-direct (`f32::from_le_bytes`/`to_le_bytes`, which fold to plain little-endian loads/stores). These kernels stay per-backend so both call the *same* Rust code and the differential pins the result.

The layout is visible in the editor: hovering a packed value or list shows the storage fact (`@packed — 12 bytes`; `flat packed storage — 12 bytes/element, column-major (SoA)`), and inlay type hints mark it compactly (`: List<Vec3> · flat`, `· SoA`) — see [Editor and AI Tooling](Editor-and-AI-Tooling).

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

- **Explicit SIMD intrinsics** (`Simd<T, N>` + const generics) as a later track — the shipped approach deliberately relies on layout (the SoA columns above) + autovectorization instead, which benched *faster* than hand-written intrinsics.

Two items formerly listed here have since resolved: zero-copy cross-thread **borrow-share** for isolates shipped (interned `&'static` shapes + the `SharedRegion` spawn path — see [Concurrency Internals](Concurrency-Internals)), and the `gc-arena` tracing idea was superseded — cycle collection ships as the refcount GC's backup trace + trial-deletion collectors (see [Memory Management](Memory-Management)).

## Finding the hot spots

Before optimizing, measure: [`noeta profile`](Profiling) reports where a program spends its time — an
exact per-function call-count/self-time table (`--instrument`) or a wall-time **flamegraph** (SVG /
speedscope). It profiles the production VM tier-0, so it shows the language-level shape of a run
(which function/line is hot); see [Profiling](Profiling) for what tier-0 does and does not reflect.

## See also

- [Profiling](Profiling) — find where time goes (hot-function table / flamegraph).
- [The Virtual Machine](The-Virtual-Machine) · [Memory Management](Memory-Management) — the two biggest performance stories, in depth.
- [Fixed-Width Integers](Fixed-Width-Integers) — the packed value types the numeric layout builds on.
