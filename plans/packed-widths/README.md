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

Slices 1 and 2 are **done and merged** (arc `c822c181`, combined gate 7/7 including
`differential_backends_agree`). Slice 3 is optional and deferred — see below.

| # | Slice | State | Notes |
|---|---|---|---|
| 1 | Packed storage for all widths | ✅ done | `PackedKind::IntN{bits,signed}` + `F64`; natural `byte_width`; `packed_layout` gate; `from_bytes`/`to_bytes` with sign/zero extension; reflect path so `List<iN>`/`List<f64>` are distinct (via construction tag). **39 B/element vs 73** on a 10-field mixed struct. FORMAT_VERSION → 5. Reifies width **only in container-element position** (a top-level scalar `i32`/`f64` still erases, preserving `params_of`/`type_of` agreement). |
| 2 | f32 scalar narrowing + erased-width warning | ✅ done | `NarrowTarget::F32` + `F32 <: float` in both matchers; `.as<T>()` honors the edge. **E0063** warns on scalar `is iN`/`is f64`; `is f32`/`is int`/`is List<iN>` do not. |
| 3 | SIMD/kernel acceleration where it pays | ⏸ deferred | Optional. Slice 1 delivers storage + reflection — the reasons the width exists. Slice 3 only vectorizes *ops* on the new-width buffers, and only where a kernel exists (today f32/vector-shaped). No correctness or fidelity gap without it; everything runs scalar-ly. Scope against a concrete workload rather than speculatively. |

### Deferred as a separate arc (not a loose end here)

**Bare-scalar-list compaction** — `List<i32>` as a *raw* 4-byte buffer, versus `List<@packed struct>`.
No primitive has this today, not even `f32` (`packed_layout` requires the element to be a declared
`@packed` struct). So it is a distinct capability for *all* primitives, needing a scalar-element
materialization mode in both value backends — genuinely its own arc. The reflection distinctness it
would enable is already delivered here via the construction tag; only the compact *storage* of a
bare scalar list is outstanding.

This arc branches off `arc-cli-foundations` (unmerged) because slice 2 uses the `BuiltinTy` enum
that arc introduced — doing the f32 narrowing anywhere else would add a scattered case `BuiltinTy`
exists to eliminate. The two arcs merge to main in sequence.
