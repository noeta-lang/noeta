# Packed reductions

The packed-widths arc made `List<i32>` a compact 4-byte buffer — but **the storage win is not a
throughput win**. No list op reads the buffer as bytes: `sum` is an `IterMethod`, `ListMethod` has no
reductions at all, so `List<i32>.iter().sum()` iterates and **materializes every element** into a
`Value` before folding. This slice makes a reduction over a packed scalar list fold the raw buffer in
a tight (autovectorizable) loop, so the compact representation finally pays off in speed.

This is the reframed, useful core of the deferred "SIMD slice 3". Note the existing `vec.Kernels`
are **not** explicit SIMD — they are `chunks_exact` buffer loops that LLVM autovectorizes. We match
that: write the buffer fold, let LLVM vectorize, measure. Do **not** reach for `std::simd`/portable
SIMD (unstable, toolchain-pinned) unless a measured hot loop demands it.

## Deliverable (this slice)

Buffer-direct **reductions** over a packed scalar list, with a correct scalar fallback for boxed
lists (so the method works on any numeric list, just faster when packed):

- Numeric: `sum`, `min`, `max`, `product`.
- `List<bool>`: `any`, `all`, `count`.

Core priority is `sum`/`min`/`max`; `product` and the bool trio should fall out of the same
buffer-fold machinery (a fold with a different combiner) — include them if the blast radius stays
flat, else note as a follow-on.

## Settled decisions

1. **Integer overflow — wrap at width.** A reduction is a repeated binary op, so it must match scalar
   `+`/`*` (the `WideInt`/`MaskWidth` semantics): `sum` over `List<i32>` wraps at 32 bits, exactly as
   folding with `+` would. Consistency over surprise. A `checked_sum() -> ?T` variant is a reasonable
   follow-on, not required here.
2. **Float determinism — one shared kernel, called by both backends.** A vectorized fold reassociates
   additions, so it can round differently from a sequential fold — accepted for `sum`. The hard
   requirement is that both backends produce **byte-identical** floats or the differential oracle
   fails. Resolve it the way `vec3.rs` already does: the kernel body lives once in `noeta-stdlib` and
   BOTH backends call it, so the reduction order is identical by construction. The packed path and the
   boxed-fallback path must also agree for a given list type (they will: a `List<f32>` is always
   packed, a `List<i64>` always boxed — the representation is consistent within a type).

## Where it belongs

**Not `std.vec`** — that is graphics (Vec3/Quat). These are general array primitives, so they attach
as reductions on `List<T>` for a packed-scalar `T`, dispatched to a buffer kernel when the list is
packed (post-arc: the sub-8-byte widths + `f32`) and to the ordinary scalar fold when boxed. Kernel
bodies live in `noeta-stdlib` beside `vec3.rs`, wired through the existing `NativeCtx::with_packed*`
seam. The `KindKey` gating vocabulary already knows every width from the arc — only the kernel bodies
are new.

**Open design point the agent must scope and report:** the attachment point. `sum` already exists as
an `IterMethod` (`xs.iter().sum()`). Options: (a) direct `List<T>` reductions with a packed fast path
(simplest to make fast; slight surface overlap with `iter().sum()`), or (b) specialize the iterator
reduction to detect a packed source. Lean (a) for simplicity, but whichever is chosen, `iter().sum()`
and the packed reduction must give identical results — no divergence.

## Follow-ons (not this slice)

- Element-wise bulk ops (`List<T> + List<T>`, `scale`, `abs`) — the array-programming layer.
- Integer/small vector types (`IVec2`/`IVec3`, `Color` as `u8x4`) — extend `std.vec` + `vec.Kernels`.
