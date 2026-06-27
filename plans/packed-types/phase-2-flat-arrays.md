# P-PACK Phase 2 — flat typed arrays (`List<packed>`)

**Status: in progress.** The first *big measurable* win of P-PACK: a list whose element type is a
packed struct is stored as **one contiguous raw-primitive buffer**, not N boxed objects + N pointers.
Branch `packed-types` (continues from Phase 0 `e9ce8a2` + Phase 1 `ab9074c`).

## Settled decision (with user): the raw primitive buffer

A flat `List<Vec3>` stores its elements as **raw primitive bits in a contiguous `Vec<u64>`** (one
machine word per primitive field, pre-Phase-3), interpreted through the packed shape — **not** an
inline `Vec<Value>` of boxed/tagged slots. This is the true P-PACK layout:

- Phase 3 (`f32`) just narrows the slot width — no rework.
- Phase 4 (SIMD) maps kernels straight onto the buffer — no FFI seam, no rework.
- Eval gets a real memory win too (no fat `Value` enum per slot).

Cost (accepted): every **read** materializes a packed value from `(shape, words)`; every **write**
packs fields back to words. That pack/unpack layer is the core of the phase.

## Load-bearing principle (unchanged): layout is INVISIBLE to `RunResult`

A flat list must **clone / index / iterate / compare / display / JSON-encode** byte-identically to the
boxed equivalent. So:
- The representation is a pure perf detail; the differential stays green by construction.
- It can land **one backend at a time** (eval flat while the VM is still boxed → both still produce
  identical observable values). Temporary *perf* asymmetry, never *behaviour*.
- Any list op not yet specialized **materializes to a boxed `Vec<Value>`, runs the existing code, and
  (if it produces a list) may re-pack** — so every slice is green and correct; specializing more ops
  is a pure *perf-coverage* expansion, never a correctness gate.

## The channel: `resolve_packed_list_sites` (mirrors `resolve_type_of_sites`)

The checker already feeds static types to both backends via `Checked.type_of_sites:
HashMap<Span, TypeRepr>`, consumed by span (compiler bakes `TypeOfStatic`; eval stores the map). We add
a parallel, pure map:

```
Checked.packed_list_sites: HashMap<Span, reflect::PackedLayout>
pub fn resolve_packed_list_sites(program) -> HashMap<Span, reflect::PackedLayout>
```

`PackedLayout` (new, in `lang-ast::reflect`, shared) fully describes pack/unpack so neither backend
needs field *kinds* in its own shape:

```rust
pub struct PackedLayout { pub type_name: String, pub fields: Vec<PackedField> }
pub struct PackedField  { pub name: String, pub kind: PackedKind }
pub enum   PackedKind   { Int, Float, Bool, Struct(Box<PackedLayout>) }  // nested packed flattens
```

The checker builds it from a `Type` via `packed_structs` membership + `records` (name→fields), keyed by
the **construction-site span**. Both backends consult the same map on the same program ⇒ they lay out
identically ⇒ differential holds.

**Coverage (expands over slices, never a correctness cut):** start by marking the construction
*primitive* — list **literals** (`Expr::List`, synth + check positions). Producing ops (`slice`,
`sorted`, `reverse`, `set`, `filter`, `~`) inherit the input list's representation at runtime, so they
need no span. Element-*changing* producers (`map`, function returns) get their call/method spans marked
in a later slice; until then they correctly produce a boxed list (slower, never wrong).

## Slice breakdown (green per commit; benchmark every perf slice)

- **2.1 — checker channel (no backend change).** `PackedLayout` in `lang-ast::reflect`;
  `packed_list_sites` + `resolve_packed_list_sites` + `packed_layout(ty)` in `lang-check`; record at
  both `Expr::List` arms. Unit tests (literal `List<Vec3>` populates; nested `List<Segment>`;
  `List<int>` empty; non-`@packed` struct empty). Pure front-end, behaviour unchanged. ← *this slice*
- **2.2 — eval `ListRepr` refactor (no flat logic yet).** `Value::List(Rc<ListRepr>)` with
  `enum ListRepr { Boxed(Vec<Value>), Packed(PackedList) }`; all lists still `Boxed`; route the ~40
  access sites through `len()/get(i)/iter materialized/into_boxed()` helpers. De-risks the churn
  separately from the flat logic. No behaviour change; gates green.
- **2.3 — eval flat construction + hot ops + fallback + benchmark.** A packed-literal site builds
  `ListRepr::Packed { layout, words: Vec<u64> }`; specialize index / iterate / `.count()` to read
  without full materialization; every other op falls back via `into_boxed()`. New
  `eval_packed_list` bench (build + index + sum-of-field over n), parameterized so scaling shows.
- **2.4 — VM flat arrays.** The same `Payload::PackedList` in `lang-value` + compiler emits a packed
  `MakeList` when the span is marked; specialize `ListGet`/`IterSnapshot`/`ListLen`; fallback for the
  rest. VM benchmark.
- **2.5+ — op-coverage expansion + memory benchmark.** Keep more producers flat (slice/sorted/reverse/
  set/filter/concat), mark `map` spans, peak-memory bench (flat n×words vs N objects). Settle nested
  packed-in-array edge cases. Each a pure perf-coverage step.

## Out of scope (later phases / own passes)
- `f32` / fixed-width slot narrowing → Phase 3.
- SIMD kernels + 3D-math stdlib → Phase 4.
- A general user-visible "typed array" surface beyond `List<packed>` → not planned.
