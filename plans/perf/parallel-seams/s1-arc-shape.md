# S1 — P-PAR-ARC: `Rc<Shape>` / `Rc<PackedSchema>` → `Arc` (the `Send` prereq)

## Why

Borrow-share (S2) hands a worker thread a pointer into the parent's promoted object graph.
Promoted payloads carry shape handles — `Payload::Object { shape: Rc<Shape>, .. }`,
`Payload::Enum { .. }` (`crates/noeta-value/src/heap.rs:313/317`) and
`Payload::PackedList { schema: Rc<PackedSchema>, .. }` — and any worker-side `value.shape()`
(`crates/noeta-value/src/lib.rs:2075`) clones that handle. A non-atomic `Rc` clone from two
threads is UB, so shape/schema handles must become `Arc` (or never be cloned cross-thread)
**before** any borrow-share wiring.

## The change

Mechanical swap, `std::rc::Rc` → `std::sync::Arc`, for exactly two types' handles:

- `Shape`: `noeta-object` (docs), `noeta-value` (`heap.rs` payloads + `lib.rs`
  `object`/`enum_value`/`shape()`), `noeta-vm` (`shapes: Vec<Rc<Shape>>` table at `lib.rs:325`,
  the `WrapOk(Rc<Shape>)` compiled-op payload at `lib.rs:284`, inline-cache entries
  `Vec<Option<(Rc<Shape>, u32)>>` at `lib.rs:2158`, `isolate.rs` marshal/rebuild signatures),
  `noeta-eval` mirrors.
- `PackedSchema`: `noeta-object/src/lib.rs:142` (`pub shape: Rc<Shape>` inside it too),
  `noeta-vm/src/lib.rs:329`, packed-list payloads, `map_packed` sites.

`Shape` and `PackedSchema` themselves must be (and, being immutable plain data, should already
be) `Send + Sync`; add the assertions (`static_assert`-style `fn _assert<T: Send + Sync>()`)
so a future non-`Send` field is a compile error, not a latent race.

## The gate (why this slice exists at all)

An `Arc` clone/drop is an atomic RMW where `Rc`'s was a plain increment. Shape handles are cloned
on **every object/enum construction** and on `shape()` calls (inline-cache *fills*, wrap-ok,
reflection); cache *hits* only pointer-compare. So the tax lands on allocation-heavy code.

- **Gate benches:** the M2.0 VM baselines (`crates/noeta-vm/benches/vm.rs` — `property_access`,
  `dispatch_fib`, allocation) before/after. Accept: no measurable regression (within noise).
- **Fallback if it regresses:** shapes-by-`Module`-index — heap payloads store a `u32` shape
  index + one `Arc<ShapeTable>` per heap (or resolve via the VM's table), so per-object handle
  traffic is a `Copy` index and only the table handle is atomic. More invasive (every
  `shape()`-consumer changes signature), which is why the cheap swap is tried first and the
  bench decides. **If the fallback is needed, that's a scope decision to surface, not silently
  absorb.**

## Verification

Arc-standard invariants (README). Extra weight on: miri on `noeta-value` (the `unsafe` heap
blocks now see `Arc`), leak oracle 0 (drop plumbing unchanged in shape), and the gate bench
numbers recorded below.

## Numbers

**Methodology note:** the first comparison (unpinned, machine under session load) produced
incoherent deltas (fib/28 "+54%" beside fib/20 "−7%", property_access "−48%") — the saved
baseline itself was noise-polluted. Discarded. The recorded gate is an **interleaved pinned A/B**:
both bench binaries built (pre-swap `d8c9200` vs post-swap), run back-to-back on the same core
(`taskset -c 2`), Rc run saved as the `rc-clean` criterion baseline and the Arc run compared
against it. Confidence intervals tightened from ±10% (unpinned) to well under ±1%.

### A/B result (Rc `d8c9200` → Arc, pinned, criterion median-of-change)

| Bench | Δ Arc vs Rc | Reading |
|---|--:|---|
| `vm_field_assign/{1k,2k,4k}` | **+10.3% … +12.6%** | the real tax: functional update = shape-handle clone + old-object drop = 2 atomic RMWs/assign (~4.7 ns) |
| `vm_field_assign/8k` | +23% (wide CI) | same, alloc-pressure amplified |
| `vm_member_dispatch/{1k,4k}` | +4.2% / +5.4% | cache-fill clones |
| `vm/dispatch_fib` | +3.1% | |
| `vm_recursion/fib/{20,24,28}` | +1.9% / +1.5% / +4.0% | int-only; likely codegen-layout shift + enum shape traffic |
| `vm/property_access` | +1.4% | cache hits ptr-compare, fills clone |
| `vm/allocation_list`, `vm_record_update/{1k..4k}` | −0.6% … +1.8% | within noise |

**Gate verdict: FAILS the "no measurable regression" gate as-is.** The plain swap puts an atomic
clone/drop pair on every object construction/destruction; allocation-heavy single-thread code
pays ~1–4% and the field-assign idiom pays ~11%. Per this doc's own rule, the fallback is a
**scope decision surfaced to the user** (2026-07-07, pending):

- **(a) accept the tax** — S2's fan-out win dwarfs it for parallel workloads, but it taxes all
  single-thread code and gives back a slice of P-VMT;
- **(b) shapes-by-index / shape-arena** — objects carry a `Copy` index (or a raw ptr into an
  arena whose lifetime bounds all values); zero per-object handle traffic, likely *beats* the
  Rc baseline, but a real redesign: every `Payload::Object` construction site, `Value::shape()`
  consumer, and the ad-hoc reflection/Ordering/Option shapes (`values.rs`, `lib.rs:1807-1847`)
  need an owning home (arena/registry), and `SharedRegion` must pin it for promoted graphs.

The Arc swap stays on the branch meanwhile (S2 needs `Send` shapes either way; under (b) the
arena handle — not per-object handles — becomes the `Arc`).

## S1b — the approved fallback, as shipped (2026-07-07, option (b))

**Design: process-wide dedup interner, `&'static` handles.** `noeta_object::intern_shape` /
`intern_schema` (a `OnceLock<Mutex<HashMap<Key, &'static _>>>` each): every distinct shape is
`Box::leak`ed exactly once and all requests dedup onto it, so runtime values carry a bare
`Copy` `&'static Shape` — **zero refcount traffic** on construction/update/destruction, and
`Send`/`Sync` for borrow-share free of charge (the very property S1 wanted from `Arc`, without
the per-object atomics). Growth is bounded by distinct types ever loaded (REPL re-loads dedup);
the map statics keep entries reachable, so miri's leak check stays clean.

- **Dedup key = all fields including `structural_eq`** — stricter than `Shape`'s `PartialEq`
  (which excludes it): a REPL redefinition can flip `Equatable`-ness and those shapes must stay
  distinct. `PartialEq` users see old semantics; pointer-identity users only gain sharing.
- **Interning is cold-path only** (module load, reflection); hot fixed shapes are
  `OnceLock`-cached: `make_some`/`make_none` (every optional-returning native op) and the three
  `Ordering` variants (`make_ordering` runs per comparison inside `.sorted()`).
- `PackedSchema.shape` and `PackedKind::Struct` are `&'static` too (schemas interned at module
  load, nested schemas keyed by address). `noeta-eval` untouched (own `TypeDef`-based schema).
- Inline caches store `(&'static Shape, u32)` — the "keep the Rc alive" comment is obsolete;
  a cache hit is the same pointer compare, a fill is a plain copy.
- Isolate `shape_index`/`rebuild` work across VMs by pointer identity now (interning makes the
  module's shapes literally the same objects in parent and worker).

### S1b gate result (pinned interleaved A/B, interned-static vs the Rc baseline)

| Bench | Δ idx vs Rc | Bench | Δ idx vs Rc |
|---|--:|---|--:|
| `vm/dispatch_fib` | **−15.4%** | `vm_field_assign/1k..8k` | **−3.3% … −8.0%** |
| `vm_recursion/fib/20/24/28` | −9.8% / −13.0% / −12.7% | `vm_member_dispatch/1k..8k` | −2.7% … −3.7% |
| `vm/allocation_list` | −3.0% | `vm_record_update` | −1.8% … −5.0% (one noisy +7.7% outlier at /2000 against the trend of its three neighbours) |
| `vm/property_access` | −1.8% | | |

**Gate verdict: PASSES — and beats the Rc baseline everywhere.** Eliminating the per-object
handle traffic removed not only the would-be Arc atomics but the *pre-existing* Rc inc/dec on
every object construction/copy/destruction. `Send` shapes for S2 came free. Conformance 496,
differential 0-skipped/agree, workspace 69 suites, miri (noeta-value 50, noeta-gc 5) all green.
