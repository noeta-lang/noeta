# P-PACK — packed value types & flat typed arrays

**Status: planning.** The milestone-scale layout track the perf sweep cleared the runway for
(`plans/perf/README.md` calls it out as unblocked-but-out-of-scope-for-the-sweep). Design source:
architecture §3.1 (`docs/resources/01-architecture.md`), surface §packed
(`docs/resources/02-syntax.md`), implementation-plan M2 §packed
(`docs/resources/03-implementation-plan.md`).

## Goal

A `struct` whose fields are all primitives (or other packed structs) can be **packed** — laid out
**unboxed and contiguous** (no header, no shape, value semantics, passed by value) — and a
`List<packed>` is stored as a **flat contiguous buffer**, not an array of boxed-object pointers. This
is the layout games / numerics / ECS want, and the foundation SIMD needs. The operator-trait surface
(already shipped) keeps the syntax elegant: `position = position + velocity * dt`.

```
packed struct Vec3 { x: f32; y: f32; z: f32 }     // unboxed, 12 bytes flat
points: List<Vec3> = [ … ]                         // flat buffer, not pointers
```

## The load-bearing constraint: layout is INVISIBLE to `RunResult`

The differential oracle asserts the two backends (NaN-boxed VM, tree-walker eval) produce
byte-identical `RunResult`. A packed value must **clone / compare / display identically** to the
boxed equivalent — the flat layout is a pure implementation detail. This is exactly the perf-sweep
posture: an optimization can land in **one backend first** (a temporary *perf* asymmetry, never a
*behaviour* asymmetry) and the differential stays green. It is what lets this huge track be sliced
safely: every slice is observably a no-op.

## Current-state seams (from the value-representation audit)

| Concern | VM (`lang-value`, NaN-boxed) | Eval (`lang-eval`, tree-walker) |
|---|---|---|
| struct value | `Payload::Object { shape, slots: Vec<Value> }` — **slot-ordered** | `Value::Object(Rc<ObjectValue>)` — fields are a **name-keyed `BTreeMap`** |
| list | `Payload::List(Vec<Value>)` — boxed words | `Value::List(Rc<Vec<Value>>)` — boxed enum values |
| floats | f64 only | f64 only |
| fixed-width nums | none | none |
| `packed`/`f32` keyword | none | none |
| FFI / SIMD seam | none (pure Rust) | none |

Key asymmetry: the **VM already stores struct fields slot-ordered**; **eval stores them name-keyed**.
Aligning eval onto a slot/flat model is the main internal groundwork for a real packed layout.
`Shape` (`lang-object`) already carries `fields: Vec<String>` in slot order and a `structural_eq`
flag — ready to grow an `is_packed` bit. Equality/display already iterate slots in declared order in
both backends, so a flat layout that preserves slot order is observationally identical.

## Phased decomposition (ascending in risk; each phase differential-safe by construction)

**Phase 0 — surface + the packed constraint (semantic marker; NO layout change).**
`packed struct` parses; `StructDecl.is_packed`; the checker gates a packed struct so every field is a
primitive (`int`/`float`/`bool`) or another packed struct — never a string/list/map/class/enum/`dyn`/
unbounded generic (new diagnostic **E0038**). Packed structs otherwise behave exactly as the
value-`struct`s they already are — same `slots`/`BTreeMap` representation, zero runtime change, both
backends identical by construction. This is the foundation: the language gains the concept, fully
validated, at zero risk. *(No perf yet — it is groundwork, so it is the one slice not benchmark-gated.)*

**Phase 1 — flat packed-value representation (the unboxing; one backend at a time).**
A packed struct value stored as a flat primitive buffer (raw little-endian bytes, or a typed
`Vec<f64>`/`Vec<i64>` slot vector) instead of boxed `Value` slots. Eval is realigned off the
name-keyed `BTreeMap` for packed types. Pure-internal; clone/eq/display reproduce the boxed output
exactly. Benchmarked (allocation + access). VM and eval can land in either order (temporary perf
asymmetry tolerated).

**Phase 2 — flat typed arrays (`List<packed>`).** The first big measurable win: `List<Vec3>` as one
contiguous buffer rather than N heap objects + N pointers — eliminates per-element allocation and
indirection. Needs a generics-specialization carve-out: the checker knows the element type is packed,
so both backends pick the flat list representation. Benchmarked (peak memory + iteration throughput),
parameterized over n so the *scaling* shows.

**Phase 3 — fixed-width numerics (`f32`, and the family as far as needed).** Halves float memory and
makes types SIMD-amenable. Type lattice (`Type::F32`/a width-parameterized numeric), literal
inference, both backends (NaN-box has room in the float space; eval needs a variant). Sized so
`packed struct Vec3 { x: f32, … }` lays out 12 bytes. *Scope (just `f32`, the float pair, or a full
i/u/f family) decided when we reach it — `f32` is the only one the Vec3/SIMD case strictly needs.*

**Phase 4 — SIMD kernels + 3D-math stdlib (the throughput payoff).** Contiguous tagless buffers let a
`std`-side 3D-math module bind SIMD kernels over flat `List<f32>`/`List<Vec3>`, with the operator-trait
surface keeping `a + b * dt` elegant. **Recommend pure-Rust `std::simd`/portable-SIMD over an FFI seam**
(there is none today; building one is itself a milestone). Gated by the capability/DCE discipline — the
numeric machinery only ships when used.

## Open decisions (to settle before / as we reach each)

1. **Surface syntax (shapes Phase 0): SETTLED — `@packed` directive** (chosen with the user over the
   `packed` keyword and `@derive(Packed)`), consistent with the evolved `@derive`/`@attribute`/`@role`/
   `@semantic` directive style. No new lexer keyword (it is `@` + ident); it joins the name-based
   dispatch set as a fifth built-in decorator directive, takes no arguments (E0037 if given any, like
   `@semantic`), and is a struct-only layout marker.
2. **Fixed-width scope (Phase 3):** just `f32`, the `{f32,f64}` pair, or a full fixed-width family.
3. **SIMD approach (Phase 4):** pure-Rust portable-SIMD (recommended) vs a native FFI seam.

## First slice

**Phase 0**, on a new branch off the current line. It is bounded, differential-safe, and the
necessary foundation; the heavier phases (flat layout, flat arrays, fixed-width, SIMD) are each large
enough to plan in their own pass when reached. Verification: conformance + differential (0-skipped,
backends agree — trivially, since Phase 0 changes no runtime behaviour), leak oracle, clippy/fmt, a
conformance case proving a packed struct of primitives checks + runs, and a negative case proving a
non-primitive field is E0038.
