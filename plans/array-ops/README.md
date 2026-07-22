# Array-programming ops + vector kernels

Follow-on to packed-widths + packed-reductions. Two subsystems:

- **Array-programming layer** — element-wise bulk ops over packed scalar lists, plus the two
  reduction loose ends (`checked_sum`, narrow-typed iterator reductions).
- **Vector kernels** — integer/u8 vector shapes (`IVec2`/`IVec3`, `Color`) added to `std.vec`.

Both fold raw packed buffers the same way reductions do — tight `chunks_exact` loops LLVM
autovectorizes, **not** explicit `std::simd`. One shared kernel body per op, called by both backends,
so the differential stays byte-identical (the rule that held for reductions).

## Slice 1 — array-programming layer (`slice-array-bulk`)

**Element-wise bulk ops.** `+` on two lists is currently an *error* (`~` is concat), so the operator
is free. Define element-wise `+`/`-`/`*` on two lists of the **same numeric element type and length**
(length mismatch = runtime error), result = the operand type. Packed operands fold buffers; boxed
operands use a scalar loop; the two must agree. Integer ops **wrap at element width** (consistent with
scalar `+` and with reductions). Plus methods for the forms with no natural binary operator:
`scale(s)` (list × scalar), `abs()`, `neg()`. `clamp(lo, hi)` if it falls out cheaply.

**`checked_sum() -> ?T`** — the follow-on flagged by the reductions slice: `none` on overflow, `some`
otherwise. Numeric lists only.

**Narrow-typed iterator reductions** — the residual: `xs.iter().take(3).sum()` on a narrow list folds
unmasked at 64-bit because `Iterator<iN>.sum()` is typed `Unknown` (`crates/noeta-check/src/stdlib.rs`
~:403/:519). Make an iterator reduction over a narrow element type return that type and wrap at its
width, so it agrees with `xs.sum()`. Keep `iter().sum()` on a list-backed iterator delegating to the
same reduction kernel (already true post-reductions-slice).

Area: `crates/noeta-stdlib/src/reductions.rs` (+ a bulk-ops sibling), `crates/noeta-check/src/stdlib.rs`
(typing) + operator typing/lowering, both backends' dispatch, `NativeCtx::with_packed*`.

## Slice 2 — integer/u8 vector kernels (`slice-vec-kernels`)

`Vec3` is **structural/bring-your-own** — any struct with three `f32` fields; std ships the *kernels*
(`crates/noeta-stdlib/src/vec3.rs`, `quat.rs`), exposed as `vec` bulk ctx-functions + the
`impl vec.Kernels for T {}` bundle. This slice adds the same machinery for **integer and u8 vector
shapes**, following the vec3.rs pattern exactly (bulk buffer kernels + a Kernels bundle):

- **`IVec2` / `IVec3`** — structs of 2/3 `i32` fields. Component-wise add/sub/scale/min/max/dot.
  Integer arithmetic **wraps at width** (arc convention). Dot-product width is a decision the agent
  must make and report (i32·i32 sums overflow easily — wrap at 32, or widen the result to `int`?).
- **`Color`** — a struct of 4 `u8` fields (RGBA). **Saturating** add/sub (domain-correct: bright +
  bright clamps to 255, it must not wrap to dark — this is the one place saturating beats the arc's
  wrapping convention), plus scale (saturating). Confirm the saturating choice in the report.

Area: `crates/noeta-stdlib/src/vec3.rs` + new sibling module(s), `crates/noeta-stdlib/src/registry.rs`
(the `vec` ExtModule — new ctx-functions + bundles), typing in `crates/noeta-check`.

## Slicing rationale

Slices 1 and 2 run in parallel: distinct subsystems (list-ops/reductions vs the vec module). They
share `registry.rs` and `stdlib.rs` but edit different regions — regional conflicts resolvable at
merge, as prior arcs did. `checked_sum` + the narrow-iter fix ride with slice 1 because they are the
same reductions machinery; splitting them off would fight over `reductions.rs`/`stdlib.rs`.

## Decisions carried in (do not relitigate)
- Integer element-wise + reductions **wrap at width**; **Color saturates** (domain exception).
- One shared kernel body per op, both backends call it (differential byte-identical).
- Not explicit SIMD — autovectorized `chunks_exact` loops; measure, don't hand-roll intrinsics.
- No FORMAT_VERSION bump expected (runtime methods/operators, no serialized-shape change) — confirm.
