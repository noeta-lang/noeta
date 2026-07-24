# Scalar unification — one numeric-kernel core

Today the numeric kernels are **five files, ~3,160 lines, no shared abstraction** — `reductions.rs`,
`bulk.rs`, `ivec.rs`, `vec3.rs`, `color.rs` each roll their own per-width `read_le`/`chunks_exact`/
arithmetic via local macros. The same handful of operations, re-expressed per width, three times over
(list reductions, list element-wise ops, vector kernels). That duplication *is* the coverage gap:
adding a width means touching every file, so nobody did — vector kernels cover only i32/f32/u8-as-Color,
with **no f64 and none of i8/i16/i64/u16/u32/u64**.

This arc collapses that to **one source of truth**: a `Scalar` element trait consumed by generic
kernel bodies across all three surfaces. Adding a width becomes one trait impl that lights up every
surface at once. Same principle as the `BuiltinTy` enum — centralize the per-type knowledge.

## The `Scalar` trait (the linchpin — implement exactly this)

One Rust trait in `noeta-stdlib`, implemented once per numeric type
(`i8 i16 i32 i64 u8 u16 u32 u64 f32 f64`). It is the single source of per-element-type behavior:

```rust
pub trait Scalar: Copy {
    const BYTES: usize;                 // packed width: 1, 2, 4, 8
    type Wide;                          // dot/product/sum accumulator: iN/uN -> i64/u64, f32 -> f32, f64 -> f64
    type Float;                         // length/sqrt result: integer -> f64, f32 -> f32, f64 -> f64
    fn read_le(bytes: &[u8]) -> Self;   // decode BYTES little-endian (sign/zero-extend per type)
    fn write_le(self, out: &mut Vec<u8>);
    fn add(self, o: Self) -> Self;      // DEFAULT arithmetic: wrap for int, IEEE for float
    fn sub(self, o: Self) -> Self;
    fn mul(self, o: Self) -> Self;
    fn sat_add(self, o: Self) -> Self;  // SATURATING mode (SatKernels): clamp for int, == add for float
    fn sat_sub(self, o: Self) -> Self;
    fn min(self, o: Self) -> Self;      // total order (NaN policy: match existing float min/max)
    fn max(self, o: Self) -> Self;
    fn abs(self) -> Self;               // unsigned: identity
    fn neg(self) -> Self;               // unsigned: wrapping negate (or document)
    fn widen_mul(self, o: Self) -> Self::Wide;   // for dot: widen then multiply
    fn wide_add(a: Self::Wide, b: Self::Wide) -> Self::Wide;  // accumulate in the wide type
    fn to_float(self) -> Self::Float;            // for length/normalize
}
```

Exact semantics (settled — the arc conventions):
- **Default arithmetic wraps** for integers (matches scalar `+`, reductions, element-wise ops); **IEEE**
  for floats. **Saturating** is the opt-in mode, integers only meaningfully (float sat == plain).
- **`Wide`**: integers accumulate in i64/u64 (dot/sum can't wrap silently), f32/f64 stay themselves.
- **`Float`**: integers promote to f64 for length/sqrt; f32 stays f32; f64 stays f64.
- Signed vs unsigned differ in read_le extension, min/max, sat bounds, neg — all captured in the impl.

## Consumers — all three become thin generics over `Scalar`

- **Reductions** (`reductions.rs`): `sum`/`product`/`min`/`max` over a buffer become `fn f<S: Scalar>`;
  `sum`/`product` accumulate in `S` (wrap) or return `checked`; `dot`-style already widens via `S::Wide`.
- **List element-wise** (`bulk.rs`): `add`/`sub`/`mul`/`scale`/`abs`/`neg`/`clamp` become `fn f<S: Scalar>`
  zip/map bodies.
- **Vector kernels** (was `ivec.rs`/`vec3.rs`/`color.rs`): per-lane `S` ops over a fixed-arity packed
  struct + cross-lane reductions (`dot -> S::Wide`, `length -> S::Float`).

The Rust compiler monomorphizes one body per width. The per-width macros in all five files go away.

## Unifying the vector bundles (needs the ABI change below)

Three bundles today (`vec.Kernels` f32 / `IntKernels` i32 / `ColorKernels` u8) exist ONLY because the
ABI can't express an **element-relative return type** (`dot` -> f32 for a float vec, -> int for an int
vec). Collapse to **two bundles by *semantics*, not by type**:
- **`vec.Kernels`** — default arithmetic (wrap int / IEEE float), for ANY uniform numeric shape.
- **`vec.SatKernels`** — saturating, for integer/u8 shapes. `Color` becomes `impl vec.SatKernels for Color {}`.

Both generic over the element type via the ABI change.

### ABI change — element-relative return types
`RetTy` gains variants referencing the constrained shape's element type, resolved by the checker at the
`impl vec.Kernels for MyStruct {}` site from the struct's concrete field type:
- `RetTy::Elem`      — the scalar element (e.g. `scale(s: Elem)`)
- `RetTy::ElemWide`  — the element's `Wide` (e.g. `dot() -> ElemWide`)
- `RetTy::ElemFloat` — the element's `Float` (e.g. `length() -> ElemFloat`)
- (`RetTy::SameAsArg(0)` already = the vector itself)

The constraint accepts a **uniform numeric field of any (kind, width, signedness)** — generalize
`ConstraintField` beyond the pinned `IntN{32,signed}`/`F32`.

## Coverage after the arc
Every width lights up for free — all integers (i8..u64, signed+unsigned), f32, f64 — across reductions,
element-wise ops, and vector kernels. Plus the incidental gaps close (f32 bundle `min`/`max`/`abs`, bulk
`min_all`/`max_all`) since they are just more generic bodies. "Which types are covered" becomes "which
types implement `Scalar`" — and the answer is all of them.

## Slices
| # | Slice | Branch | Depends |
|---|---|---|---|
| 1 | `Scalar` trait + migrate `reductions.rs` onto it (validates the trait) | `slice-scalar-core` | — |
| 2 | ABI element-relative `RetTy` + constraint generalization + checker resolution | `slice-abi-elem-retty` | — |
| 3 | Unify vector bundles → `vec.Kernels` + `vec.SatKernels`, all widths + f64 | `slice-vec-unify` | 1, 2 |
| 4 | Migrate `bulk.rs` (list element-wise) onto `Scalar` | `slice-bulk-migrate` | 1 |

Wave 1: slices 1 + 2 (parallel — different crates). Wave 2: slices 3 + 4 (parallel — different files).

## Invariants (do not regress)
- Both backends byte-identical (one shared kernel body → differential holds). Every kernel is Rust
  generic monomorphized once; no per-backend copy.
- Integer default arithmetic wraps; float IEEE; saturating is opt-in. Equality stays width-blind.
- NOT explicit SIMD — the generic bodies stay `chunks_exact`-shaped so LLVM autovectorizes each mono.
- No behavior change for existing f32/i32/u8 kernels or the reductions/bulk surfaces — this is a
  refactor + extension, pinned by the existing fixtures plus new-width ones.
- `.noeb` FORMAT_VERSION: confirm per slice (bundle/constraint changes are compile-time; likely no bump).
