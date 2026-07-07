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

_Before/after gate-bench table to be recorded here._
