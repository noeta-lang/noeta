# Packed widths arc

Fixed-width numerics exist but only **f32** is honored at runtime. Every other specified width —
`i8 i16 i32 i64 u8 u16 u32 u64` and `f64` — is *erased* to the 8-byte `int`/`float` everywhere:
scalar values and packed buffers alike. `List<i32>` is a boxed list of 8-byte ints, byte-identical
to `List<int>` (`List<i32> is List<int>` → true, both directions). So declaring `i32` today buys
compile-time range/overflow semantics and **nothing** at runtime — no storage compaction, no speed.

That is the gap this arc closes: **pack every specified width at its natural size**, the way `f32`
already packs to 4 bytes. `List<i8>` → 1-byte buffer, `List<i32>` → 4-byte, `List<u16>` → 2-byte,
`List<f64>` → 8-byte (distinctly typed even though same size as `float`). This is where the
performance reason to use a fixed width becomes real, and it reifies the width **exactly where it is
physically meaningful** (packed storage) rather than on a boxed scalar where remembering it would
cost an allocation for no speed benefit.

## Two capabilities, kept separate

"Pack" bundles two things the code should not couple:

- **Compact storage** — `byte_width` at natural size. Uniform, the real win, done for *every* width.
  Reifies packed-element width as a free consequence: `List<i32>` becomes genuinely distinct from
  `List<int>`.
- **SIMD/kernel acceleration** — the vec kernels are f32/vector-shaped and will not auto-vectorize
  `i8`/`i32` lanes. Compact storage does **not** require it — a 4-byte `i32` buffer works read
  scalar-ly. Add kernels only where they pay; never gate storage on kernel coverage.

## Decisions (settled — do not relitigate in a slice)

1. **Signedness** — `PackedKind` carries none today (its only int is 8-byte). Sub-64-bit ints need
   `(bits, signed)` so read-back sign-extends correctly → `PackedKind::IntN { bits, signed }`.
2. **The 8-byte widths** (`f64`, `i64`, `u64`) — pack them too, for uniformity: "every width packs
   at its width" is a cleaner rule than "every width except the 8-byte ones," and it makes packed
   reflection uniform. Cheap, since same size; only `u64` has a genuine range difference.
3. **Alignment** — tight packing, **no** auto-padding. This is already the rule: `field_prefix` is
   the sum of prior `byte_width`s and a 1-byte `Bool` already sits next to an 8-byte `Int`. `@packed`
   means packed; mixed widths continue tight. SIMD on unaligned data uses unaligned loads (perf, not
   correctness).

## Scalar-dyn reflection stays erased — deliberately

Packing changes *buffer* storage, not scalar boxing. A scalar `i32` in a `dyn` is still an immediate
int with no room for a width tag (the NaN-box int immediate uses all 48 payload bits; there is no
boxing site to stamp one, and `Value::reflect()` is pointer-only). Reifying scalar widths would mean
heap-boxing every dyn scalar — an allocation on the dynamic path, for introspection with no speed
payoff. So `x is i32` on a scalar stays false, handled by a **checker warning** (slice 2), while
`List<i32> is List<i32>` becomes genuinely distinct (slice 1).

## Slices

| # | Slice | Depends on | Notes |
|---|---|---|---|
| 1 | Packed storage for all widths | — | `PackedKind::IntN{bits,signed}` + `F64`; `byte_width`; `packed_layout` gate (stop sending `IntN`→`None`); `KindKey`; `from_bytes`/`to_bytes`; the reflect path so `List<iN>`/`List<f64>` element widths are distinct; `.noeb` FORMAT_VERSION |
| 2 | f32 scalar narrowing fix + erased-width warning | — | `f32 is f32`/`is float` wrongly false (reified but no `NarrowTarget` head) → add head + `F32 <: float`. Warn on scalar `is iN`/`is f64` (statically always-false; "erases to int/float — did you mean `is int`?"). `is f32` and `is List<iN>` do NOT warn. Runs parallel to slice 1 (narrowing machinery vs packed layout — disjoint files) |
| 3 | SIMD/kernel acceleration where it pays | slice 1 | Only widths with a real kernel; scalar fallback otherwise |

Slices 1 and 2 are independent and run in parallel. Slice 3 follows slice 1.

This arc branches off `arc-cli-foundations` (unmerged) because slice 2 uses the `BuiltinTy` enum
that arc introduced — doing the f32 narrowing anywhere else would add a scattered case `BuiltinTy`
exists to eliminate. The two arcs merge to main in sequence.
