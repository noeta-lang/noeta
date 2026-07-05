# P-NUM-SYM — symmetric numeric tiers (add `f64`, make `f32` strict)

**Status: DONE** (S1+S2 in one commit). Branch `numeric-symmetry` off main. Gates all green:
conformance 464, differential 0-skip/agree, jit-differential 453/0-div/0-leak, workspace 711,
CLI 59, miri 55, clippy+fmt clean.

**Literal ergonomics now DONE too** (follow-up commit): the `f64` suffix (`1.5f64`) and bare-float→
`f32` adaptation both landed — the two tiers are symmetric down to literal syntax. See the
"Literal ergonomics" section below.

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

## Literal ergonomics (done, follow-up commit)

Both float literal escapes now match the integer tier:

- **`f64` suffix** (`1.5f64`, `5f64`) — full lexer/parser/AST (`Expr::F64`) plumbing; lowers to a
  plain `Const::Float` (f64 ≡ float). This is the expression-position escape (the bare-literal
  adaptation only fires where a type is expected).
- **Bare-float → `f32` adaptation** (`mut x: f32 = 1.5`) — type-directed: the checker records the
  adapted literal's span in `f32_literal_sites` (a new `LoweringSites`/`Checked` field, threaded
  like `width_sites`), and lowering narrows it to `Const::F32(v as f32)`. Both backends share the
  lowering, so the differential agrees (verified). `f32` precision matches the suffixed form for
  ordinary literals (double-rounding via `as f32` is a rare theoretical ULP edge, deterministic and
  differential-safe).

### Argument-position adaptation (done)

A bare literal now also adapts into a fixed-width **parameter** — `f(200)` for a `u8` param,
`f(2.5)` for `f32`/`f64` — for functions and methods, across the whole fixed-width tier
(`i8`…`u64`, `f32`, `f64`). Previously this was a uniform limitation (literals adapted at bindings
but not at call args). Mechanism: the binding and argument paths now share one
`try_adapt_literal(expr, expected) -> Option<Type>` helper (extracted from the binding-check arms),
which range-checks an `IntN` (E0044) and records the `f32` narrowing site. `check_args` threads the
argument **expressions** (a new `&[Expr]` parameter, plumbed through `synth_call` /
`call_user_method` / `check_method_args`) and tries adaptation before the type-based
`arg_assignable` — so the `int`/`float` value-widening leniency is preserved, and only *literals*
adapt (a non-literal int *value* into an `i64` param is still E0007).

**Generic calls too (done).** `check_generic_call` has its own argument loop (it must infer the
type parameters from the argument types first), so it was the one path that bypassed `check_args`
and thus the adaptation. It now runs the same `try_adapt_literal` against each *substituted*
parameter type before `arg_assignable` — so a literal adapts into a concrete fixed-width param of a
generic function/method (`g<T>(a: T, b: u8)` accepts `g(x, 200)`), and also into a type variable
already bound to a fixed-width type by an earlier argument (`max2<T>(100i8, 9)` binds `T = i8`, so
the `9` narrows). A genuinely-generic `T` param is untouched (its substituted type is
`int`/`float`/etc., which `try_adapt_literal` ignores), so inference is unaffected.

## Gates (per slice)

conformance + differential (0-skipped) + leak + jit-differential (+anomaly) + miri (value/gc) +
clippy + fmt. New E0044 float cases get conformance coverage.
