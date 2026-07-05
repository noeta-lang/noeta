# P-NUM-SYM — symmetric numeric tiers (add `f64`, make `f32` strict)

**Status: DONE** (S1+S2 in one commit). Branch `numeric-symmetry` off main. Gates all green:
conformance 464, differential 0-skip/agree, jit-differential 453/0-div/0-leak, workspace 711,
CLI 59, miri 55, clippy+fmt clean.

Deferred (noted below): bare-float→`f32` literal adaptation and an explicit `f64` literal suffix —
both literal-ergonomics only; the type structure is symmetric now.

## Decision (user, 2026-07-05)

The integer and float tiers should be **symmetric**:

- **Widening (lattice) defaults:** `int`, `float` — ergonomic, coerce/widen freely.
- **Strict fixed-width:** `i8`…`i64`, `u8`…`u64`, **`f32`, `f64`** — no implicit widening; mixing with
  anything else (including the widening default) is `E0044`, convert explicitly.

This keeps `i64` (strict, already exists) and **adds `f64`** as its float twin, and moves **`f32`
out of the widening lattice** so it is strict like `i32`. Rejected: aliasing `int`≡`i64` /
`float`≡`f64` (an alias can't carry a different coercion rule — see the session finding).

## Why it's contained

`int`/`i64` and `float`/`f64` are **bit-identical at runtime**; the *only* difference is
compile-time coercion. So:

- `f64` reuses `float`'s runtime representation (`Value::float`, 64-bit) — an `f64` value **is** a
  float value. No backend arithmetic changes; the VM/JIT float path already handles it.
- `f32` strictness is checker-only — the 32-bit runtime path is unchanged; the checker simply stops
  widening it.

## Slices

- **S1 (types crate).** Add `Type::F64`. `is_numeric` → `Int | Float` (drop `F32`);
  `is_arith_numeric` → `+ F64`; `numeric_rank` → `Int(0), Float(1)` (drop `F32` → both fixed floats
  rank `None`). `from_ref` `"f64"` → `F64`; `Display` `F64` → `"f64"`. Exhaustive `Type` matches get
  an `F64` arm.
- **S2 (checker).** Strict fixed-width **float** arithmetic/comparison: intercept `F32`/`F64` like
  `IntN` (same-type → that type, else `E0044`). `f64` **float-literal adaptation** (`mut x: f64 =
  1.5` — representation-free since `f64` ≡ `float`). Type `to_f64()` as `F64` and `to_float()` as
  `Float` (the strict/widening distinction). Update `E0044` wording to cover floats.
- Backends: unchanged for arithmetic (f64 = float path; f32 unchanged). `to_f64`/`to_f32` already
  exist in the conversion tower.

## Deferred

- **Bare-float → `f32` literal adaptation** (`mut x: f32 = 1.5` without the suffix). Needs
  type-directed 32-bit lowering in both backends (f32 is a *distinct* representation, unlike f64).
  `f32` keeps requiring its `1.5f32` suffix — the status quo, no regression. A follow-up ergonomic.
- An explicit `f64` literal suffix (`1.5f64`) — adaptation + annotation cover the common case.

## Gates (per slice)

conformance + differential (0-skipped) + leak + jit-differential (+anomaly) + miri (value/gc) +
clippy + fmt. New E0044 float cases get conformance coverage.
