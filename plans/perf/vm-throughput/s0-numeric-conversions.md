# S0 — Numeric conversion tower (P-VMT-CONV)

**Status: DONE.** Shared `NumScalar`/`NumConvert`/`num_convert` in `lang-stdlib` (int↔f64↔f32,
float→int saturating, NaN→0); checker types the new methods on `int`/`IntN`/`float`/`f32`; both
backends dispatch through the shared converter (agree by construction). Conformance
`types/float_conversions.lang`; differential 419 matched / 0 skipped / agree; corpus 430 passed; docs
updated. The SoA data-building idiom `(i % 100).to_f32()` now works.

**Goal.** Close the gap that there is **no explicit conversion between the integer domain and the
float domain**, in either direction. Today only int↔fixed-width-int conversions exist; the only
bridge to `float`/`f32` is mixed-arithmetic coercion, so you cannot build `f32`/`float` data from a
computed integer without a literal. (Discovered while writing the SoA benchmark — `(i % 100).to_f32()`
does not exist.)

## Evidence (current behaviour)

```
(5).to_float()      => E0007 type `int` has no method `to_float`
(5).to_f32()        => E0007 ... no method `to_f32`
(5).to_f64()        => E0007 ... no method `to_f64`
(3.7).to_int()      => E0007 type `float` has no method `to_int`
(3.7).to_i32()      => E0007 ... no method `to_i32`
y: float = x        => E0007 expected `float`, found `int`   (no implicit widening either)
5 + 2.0             => 7.0    (mixed arithmetic DOES coerce — the only existing bridge)
```

## Scope — the conversions to add

Complete the tower so every numeric type reaches every other explicitly:

| From | New methods | Semantics |
|---|---|---|
| `int`, fixed-width int | `to_float()` / `to_f64()`, `to_f32()` | value→nearest float (`i as f64` / `i as f32`) |
| `float` (f64) | `to_f32()` | narrowing round-to-nearest (`f as f32`) |
| `float`, `f32` | `to_int()` / `to_i8`…`to_u64` | truncate-toward-zero, **saturating** (Rust `as`, post-1.45); NaN→0 |
| `f32` | `to_float()`/`to_f64()` | widening (exact) |

`to_float` and `to_f64` are the same runtime op (like `to_int`/`to_i64` today) but the checker keeps
their static spelling distinct. Identity conversions (`float.to_f64()`, `f32.to_f32()`) are allowed
(uniformity), same as `int.to_int()`.

## Approach

The integer side already has the machinery — mirror it for floats. Reuse the existing
`IntMethod::Convert { signed, bits }` decode pattern (`crates/lang-stdlib/src/lib.rs`
`conversion_from_name`), adding a float-destination arm and a float-*receiver* method family.

1. **`lang-stdlib`** — extend the conversion decode so `to_f32`/`to_f64`/`to_float` decode to a new
   `Convert` destination (or a sibling `FloatConvert`) and float-receiver `to_<int>` decode to the
   existing integer destinations. Keep the actual `x as f32` / `f as i64` (saturating) casts in one
   shared helper so both backends call it — the conversion is then **agree-by-construction**.
2. **`lang-types` / `lang-check`** — type the new methods: `int.to_f32() : f32`, `int.to_float() :
   float`, `float.to_int() : int`, etc. This is where `to_float` vs `to_f64` static spellings diverge
   even though they share a runtime op (the checker already does this for `to_int`/`to_i64`).
3. **`lang-vm`** (`methods.rs`) and **`lang-eval`** — dispatch the new method names to the shared
   `lang-stdlib` cast helper. The internal casts already exist (`value.as_int().map(|i| i as f32)` in
   `methods.rs:503`, `Value::Int(i) => *i as f32` in `eval/lib.rs:2704`) — this exposes them as
   first-class methods.

## Files

- `crates/lang-stdlib/src/lib.rs` — conversion name→op decode + shared cast helper.
- `crates/lang-types/src/lib.rs`, `crates/lang-check/src/…` — return-type typing for the new methods.
- `crates/lang-vm/src/methods.rs`, `crates/lang-eval/src/lib.rs` — runtime dispatch.
- Docs: `docs/Standard-Library.md` (int bit-methods / conversions section) and
  `docs/Fixed-Width-Integers.md` (conversions table) — add the float row.

## Validation

- **New conformance cases** under `tests/conformance/` (a `conversions/` group): int→f32/f64/float,
  float→f32, float→int (incl. truncation direction, negative, saturating overflow, NaN→0).
- **Differential coverage** — the new cases run in `--differential`; both backends share the
  `lang-stdlib` cast so they agree by construction (0 skipped stays).
- Update the SoA benchmark to build data via `(i % 100).to_f32()` instead of the f32-accumulator
  workaround (proof the gap is closed).

## Risk

Low. Additive surface; no existing behaviour changes. The one decision (float→int saturating vs
error) is called out in the arc README's open questions — proposal: match Rust `as` (saturating),
document, no new diagnostic.

## Dependencies

None. Independent warmup slice; land first so later benches can build `f32`/`float` inputs directly.
