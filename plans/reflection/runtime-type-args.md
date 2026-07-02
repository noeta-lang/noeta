# Runtime type-argument reflection — closing the fidelity-B residual

**Status: design, for build.** Follow-on to the attribute-system / reflection work. Standard commit
trailers; every slice bench-free but gated on conformance + differential (0-skipped, backends agree) +
leak-oracle residency 0 + clippy/fmt.

## The gap (precisely characterized)

`type_of(value)` and `x is T` already recover a **value's concrete type** at runtime — a scalar's
payload tag, an object's shape, and every element pulled out of a `List<dyn>`. That is *not* erased.
And the **static** path (`Op::TypeOfStatic`, driven by the checker's `type_of_sites`) already reports
a *container's or generic type's* parameters precisely, **even for an empty container** — `type_of(xs)`
where `xs: List<User> = []` is `List(User)`, and `type_of(b)` where `b: Box<int>` is `Box<int>`.

The **one residual**: when a value's *static* type is lost to `dyn` (laundered through a `dyn`
parameter/field), the **runtime** `type_of` fallback (`Op::TypeOf`) cannot recover a container's or
generic type's **type arguments** — it reports `List(Dyn)` / `Struct("Box", [])`. It is uniform across
built-in collections **and** generic user types (`struct`/`class`/`enum`). The related symptom:
`x is List<int>` / `x is Box<int>` match the **head only** (a `List<int>` value `is List<string>` is
`true`), because the runtime carries no type arguments to discriminate.

Reflecting the value's **elements** is *not* a sound recovery route: a `List<dyn>` legitimately holds
mixed elements, and inferring `List<int>` from `element[0]` would both be wrong and violate "a
`List<dyn>` stays `dyn`."

## Decision (settled with user, 2026-07)

- **All containers** (`List`, `Map`, `Set`) **and generic user types** (`struct`/`class`/`enum`)
  retain their type arguments at runtime, consistently.
- **Mechanism: a construction-time reflected-type tag on heap values** — a value carries the
  `TypeRepr` the checker resolved for **its construction site** (annotation-driven, so a `List<dyn>`
  literal is tagged `List(Dyn)` and stays `dyn`; inferred otherwise; `None` when genuinely unknowable →
  falls back to today's head-only runtime classification). Laundering through `dyn` preserves the tag,
  so `type_of` recovers it.
- **This is reflection-metadata reification, NOT perf/identity reification.** Shapes stay **shared**
  (`Box<int>` and `Box<string>` keep one shape and one method table) — no shape explosion, no change to
  method dispatch or the inline caches. Monomorphic layout specialization (distinct shapes / flat
  packed arrays) remains the separate packed-types milestone (§3.1); it is *not* in scope here.
- **The tag is invisible to value semantics.** Equality, hashing, `structural_eq`, COW/reuse, concat,
  and the packed-list fast paths **must ignore it** — two values with equal contents but different (or
  absent) tags compare/hash equal and share all the same runtime paths. This invariant is the crux of
  the whole track and is asserted by the differential + leak oracles.
- **`Type.String`-style qualified ADT kept** (not bare `String`): required for pattern-matching
  (a bare tag is a binding), avoids collision with real type names, and expresses generics as variant
  payloads (`Type.List(inner)`, `Type.Struct(name, args)`). The surface already supports it; this track
  only *feeds* it the arguments on the runtime path.
- **`is` / `as` become precise** on a tagged operand: `x is List<int>` checks the element type when the
  value carries a tag; a `dyn`/untagged operand keeps the head-only match. Removes the `is List<string>`
  surprise.

## Representation

A heap value gains an optional reflected type: `Option<Rc<TypeRepr>>` (clone = refcount bump, so value
cloning stays cheap; `None` = "untagged", the pre-track behavior). Carried on `Payload::List` / `Set` /
`Map` / `Object` (VM, `lang-value`) and the analogous tree-walker `Value`s. Set at construction from a
new checker **construction-site → `TypeRepr`** map (a sibling of `type_of_sites`), baked onto the
construction ops (`List`/`Set`/`Map`/`MakeStruct`/`MakeEnum`) by lowering, read by `Op::TypeOf` /
`eval_type_repr` and the `is`/`as` matcher. (A later optimization may intern `TypeRepr`s to a module
table and carry a `u32` id instead of an `Rc` — deferred; correctness first.)

## Slices

- **R0 — checker construction-site type map.** Record, per collection-literal and object/enum-literal
  span, the resolved `TypeRepr` (reusing the `type_of_sites` resolution). Rides the `Checked` →
  compiler/reference/eval path like the other site maps. No runtime change yet (map is unused) — pure
  groundwork, corpus byte-identical.
- **R1 — collection tags + `type_of`.** Add the `Option<Rc<TypeRepr>>` tag to `List`/`Set`/`Map`
  values in both backends; lowering bakes the R0 `TypeRepr` onto the construction op; `Op::TypeOf` /
  `eval_type_repr` read it (falling back to head-only when absent). **Value semantics invisible** —
  equality/hash/COW/concat/packed unchanged (asserted by differential + leak). Closes the collection
  residual: `type_of(launder([1,2,3]))` → `List(int)`; `type_of(launder(List<dyn>))` → `List(dyn)`.
- **R2 — generic user-type tags.** Same tag on `Object` (struct/class) + enum values; `MakeStruct`/
  `MakeEnum` carry the `TypeRepr`; `type_of` reads it. Closes `type_of(launder(Box<int>))` → `Box<int>`.
- **R3 — precise `is` / `as`.** The narrowing matcher consults the operand's tag: `x is List<int>` /
  `x is Box<int>` check arguments when tagged; untagged/`dyn` stays head-only. Removes the
  `is List<string>` false-positive. New behavior pinned; no new diagnostic code.

**Gates each slice:** conformance + differential 0-skipped / backends agree + leak residency 0 both +
clippy/fmt. R1/R2 add miri over the tagged value's heap accounting (the tag is an `Rc`, so its
retain/release must balance). A dedicated conformance family (`reflection/runtime_type_args_*`) pins
the laundered-recovery + value-semantics-invisibility (a tagged and an untagged equal value are `==`).

## Sequencing: A now, B benched afterwards (decided with user)

This track is **approach A** (per-value tag, shapes shared) — it closes the reflection gap with no
dispatch/layout change. **Approach B** (distinct shapes per instantiation → IC-guard discrimination,
the on-ramp to monomorphic flat-packed storage) is a **follow-up experiment, gated on a measured
speedup** — built only if it benches faster, exactly as the P-SIMD track treated explicit SIMD (which
lost the bench and was dropped). B is *not* required for reflection; A already delivers that. Do B
after A ships, as its own benched slice, and keep it only if the numbers justify the added shape
machinery.

## Explicitly out of scope (for A)

- Perf/identity reification (distinct shapes, IC-guard discrimination, monomorphic flat-packed storage)
  — that is **approach B / the packed-types milestone (§3.1)**, sequenced after A and gated on a bench.
  A deliberately keeps shapes shared and dispatch untouched.
- Types as general first-class values in expression position (`t = int`). Type values remain confined
  to reflection contexts (`type_of`, attribute args). Out of scope unless separately wanted.
